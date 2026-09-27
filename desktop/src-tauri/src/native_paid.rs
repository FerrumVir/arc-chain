//! Native paid inference on recovered chains, signed by the desktop wallet.
//!
//! The WebView sends a prompt and the price it accepts. This module reads the
//! chain's native context from the pinned chain host, builds the exact job the
//! chain admits, signs it with the Rust-only wallet key, journals the signed
//! transaction durably, and only then submits it. Everything after submission
//! is read from the chain: the request's receipt at
//! `/native-inference/receipt/{request_id}` is the only thing that moves it.
//!
//! Cancellation follows finality. A request the WebView has not asked this
//! module to sign exists nowhere and is withdrawn there. A signed request
//! cannot be recalled: if no block admits it before its expiry height it is
//! dropped and nothing is reserved; once admitted it either finalizes, or
//! after its expiry its reservation comes back through a refund transaction
//! signed here. Nothing in this module hides a request instead.

use crate::rpc_client::strip_0x;
use crate::{rpc_client, wallet, AppState};
use arc_crypto::{hash_bytes, Hash256, KeyPair, Signature};
use arc_types::inference_contract::{
    validator_set_commitment, InferenceDomain, InferenceJob, InferenceRequest, ValidatorMember,
    INFERENCE_CONTRACT_VERSION,
};
use arc_types::transaction::{gas_costs, NativeInferenceRefundBody, NativeInferenceRequestBody};
use arc_types::{Transaction, TxBody};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tauri::State;

type CmdResult<T> = Result<T, String>;

/// Shortest and longest expiry the app offers, in blocks past the current
/// height. The chain itself only requires the expiry to be in the future.
pub const MIN_EXPIRY_BLOCKS: u64 = 30;
pub const MAX_EXPIRY_BLOCKS: u64 = 20_000;
/// How long a signed refund holds its account nonce in the journal if it is
/// never mined. After that the refund can be claimed again. Known limit: a
/// refund has no chain expiry, so a copy still pending somewhere past this
/// point competes with whatever is signed next at the same nonce, and one of
/// the two fails. No funds are at risk: a nonce admits one transaction.
const REFUND_RESERVATION_BLOCKS: u64 = 600;
const JOURNAL_FILE: &str = "native-requests.json";
/// Held (fs2, exclusive) from a journal load through its save and the
/// submission, so a second app instance on the same data directory cannot
/// sign the same nonce or overwrite this one's journal.
const JOURNAL_LOCK_FILE: &str = "native-requests.lock";
const JOURNAL_MAX_RECORDS: usize = 256;
const JOURNAL_MAX_BYTES: u64 = 4 * 1024 * 1024;
const PROMPT_PREVIEW_CHARS: usize = 120;

pub const INPUT_CANONICAL_TOKENS: &str = "canonical_tokens";
pub const INPUT_TEST_EXECUTOR_BYTES: &str = "test_executor_bytes";
/// The input encodings the node's executors read, exactly as its
/// `/native-inference/context` states them (`NativeExecutorKind::input_format`).
/// This app encodes these and nothing else.
const CANONICAL_INPUT_FORMAT: &str = "le_u32_token_ids_without_bos";
const TEST_EXECUTOR_INPUT_FORMAT: &str = "opaque_bytes";

// ── The chain's native context ────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Execution {
    model: Hash256,
    profile: Hash256,
    generation: Hash256,
    assignment: Hash256,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeServingView {
    pub executor: String,
    pub input_format: String,
    pub tokenize_endpoint: bool,
    pub tokenizer_profile: Option<String>,
    /// The most positions (1 + prompt + max tokens) the host's executor holds
    /// for one job; it never votes on a job that needs more.
    pub max_positions: Option<u64>,
}

#[derive(Clone, Debug)]
struct ChainContext {
    domain: InferenceDomain,
    members: usize,
    executions: Vec<Execution>,
    height: u64,
    max_tokens: u32,
    max_output_bytes: u32,
    max_input_bytes: usize,
    serving: Option<NativeServingView>,
    node_version: Option<String>,
    chain_protocol: Option<u64>,
    native_only_chain: Option<bool>,
    request_admission_open: Option<bool>,
}

impl ChainContext {
    fn request_admission_error(&self) -> Option<&'static str> {
        match self.request_admission_open {
            Some(true) => None,
            Some(false) => Some("this chain is not admitting new native paid requests right now"),
            None => Some("the node does not advertise native request admission; update the node"),
        }
    }
}

fn hash_field(value: &Value, field: &str) -> Result<Hash256, String> {
    let raw = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("native context is missing {field}"))?;
    Hash256::from_hex(&strip_0x(raw))
        .map_err(|_| format!("native context {field} is not a 32-byte hash"))
}

fn u64_field(value: &Value, field: &str) -> Result<u64, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("native context is missing an exact {field}"))
}

/// Parse and check `/native-inference/context`. An `Err` says why this app
/// cannot build requests the chain would admit, in words for the user.
fn parse_context(value: &Value) -> Result<ChainContext, String> {
    if value.get("candidate_protocol").and_then(Value::as_u64) != Some(4) {
        return Err("the host does not describe a protocol-4 native context".into());
    }
    let executions = value
        .get("allowed_executions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "the host's node is older than this app and does not publish its execution \
             allowlist; update the node"
                .to_string()
        })?;
    let app_version = u64::from(INFERENCE_CONTRACT_VERSION);
    match value.get("contract_version").and_then(Value::as_u64) {
        Some(version) if version == app_version => {}
        Some(version) if version > app_version => {
            return Err(format!(
                "the node speaks native contract v{version} and this app speaks v{app_version}; \
                 update the app"
            ));
        }
        Some(version) => {
            return Err(format!(
                "the node speaks native contract v{version} and this app speaks v{app_version}; \
                 update the node"
            ));
        }
        None => {
            return Err(
                "the host's node does not state its native contract version; \
                        update the node"
                    .into(),
            );
        }
    }
    let limits = value
        .get("limits")
        .ok_or_else(|| "the host's node does not publish its native limits".to_string())?;
    let request_gas = u64_field(limits, "request_gas_limit")?;
    let refund_gas = u64_field(limits, "refund_gas_limit")?;
    if request_gas != gas_costs::NATIVE_INFERENCE_REQUEST
        || refund_gas != gas_costs::NATIVE_INFERENCE_REFUND
    {
        return Err(format!(
            "the node's native gas schedule ({request_gas}/{refund_gas}) differs from this \
             app's ({}/{}); the app and node are from different releases",
            gas_costs::NATIVE_INFERENCE_REQUEST,
            gas_costs::NATIVE_INFERENCE_REFUND
        ));
    }
    let max_tokens = u32::try_from(u64_field(limits, "max_tokens")?)
        .map_err(|_| "native max_tokens is out of range".to_string())?;
    let max_output_bytes = u32::try_from(u64_field(limits, "max_output_bytes")?)
        .map_err(|_| "native max_output_bytes is out of range".to_string())?;
    let max_input_bytes = usize::try_from(u64_field(limits, "max_input_bytes")?)
        .map_err(|_| "native max_input_bytes is out of range".to_string())?;

    let members = value
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| "native context is missing its members".to_string())?
        .iter()
        .map(|member| {
            Ok(ValidatorMember::new(
                hash_field(member, "address")?,
                u64_field(member, "stake")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let domain = InferenceDomain {
        chain_genesis: hash_field(value, "chain_genesis")?,
        recovery_epoch: u64_field(value, "recovery_epoch")?,
        validator_set_hash: hash_field(value, "validator_set_hash")?,
    };
    // The member list is the preimage of the signed validator-set hash. A
    // host whose two disagree is describing a chain nobody could admit to.
    let recomputed = validator_set_commitment(&members)
        .map_err(|error| format!("the host's validator set is invalid: {error}"))?;
    if recomputed != domain.validator_set_hash {
        return Err("the host's member list does not match its validator-set hash".into());
    }
    let executions = executions
        .iter()
        .map(|execution| {
            Ok(Execution {
                model: hash_field(execution, "model_hash")?,
                profile: hash_field(execution, "profile_hash")?,
                generation: hash_field(execution, "generation_hash")?,
                assignment: hash_field(execution, "assignment_hash")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if executions.is_empty() {
        return Err("the chain's execution allowlist is empty".into());
    }
    let serving = match value.get("serving") {
        None | Some(Value::Null) => None,
        Some(serving) => Some(NativeServingView {
            executor: serving
                .get("executor")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input_format: serving
                .get("input_format")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tokenize_endpoint: serving
                .get("tokenize_endpoint")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tokenizer_profile: serving
                .get("tokenizer_profile")
                .and_then(Value::as_str)
                .map(str::to_string),
            max_positions: serving.get("max_positions").and_then(Value::as_u64),
        }),
    };
    Ok(ChainContext {
        domain,
        members: members.len(),
        executions,
        height: u64_field(value, "height")?,
        max_tokens,
        max_output_bytes,
        max_input_bytes,
        serving,
        node_version: value
            .get("node_version")
            .and_then(Value::as_str)
            .map(str::to_string),
        chain_protocol: value.get("chain_protocol").and_then(Value::as_u64),
        native_only_chain: value.get("native_only_chain").and_then(Value::as_bool),
        request_admission_open: value.get("request_admission_open").and_then(Value::as_bool),
    })
}

/// How a prompt becomes signed input on this host, or why it cannot.
fn input_kind(serving: Option<&NativeServingView>) -> Result<&'static str, String> {
    if let Some(serving) = serving {
        let expected = match serving.executor.as_str() {
            "canonical_i8" => Some(CANONICAL_INPUT_FORMAT),
            "deterministic_test" => Some(TEST_EXECUTOR_INPUT_FORMAT),
            _ => None,
        };
        if let Some(expected) = expected {
            if serving.input_format != expected {
                return Err(format!(
                    "the host's {} executor reads input as '{}', and this app encodes only \
                     '{expected}'; the app and node are from different releases",
                    serving.executor, serving.input_format
                ));
            }
        }
    }
    match serving {
        Some(serving) if serving.executor == "canonical_i8" && serving.tokenize_endpoint => {
            Ok(INPUT_CANONICAL_TOKENS)
        }
        Some(serving) if serving.executor == "canonical_i8" => Err(
            "the host runs the canonical executor but serves no tokenizer, so this app cannot \
             encode a prompt for it"
                .into(),
        ),
        Some(serving) if serving.executor == "deterministic_test" => Ok(INPUT_TEST_EXECUTOR_BYTES),
        Some(serving) => Err(format!(
            "the host runs an executor this app does not know ({})",
            serving.executor
        )),
        None => Err(
            "the host runs no native executor; point the app at a validator that executes \
             native requests, or at your own node running one"
                .into(),
        ),
    }
}

async fn fetch_context_value(http: &reqwest::Client, host: &str) -> Result<Option<Value>, String> {
    let response = http
        .get(format!("{host}/native-inference/context"))
        .send()
        .await
        .map_err(|error| format!("could not reach {host}: {error}"))?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!(
            "{host} could not describe its native context (HTTP {})",
            response.status()
        ));
    }
    response
        .json()
        .await
        .map(Some)
        .map_err(|error| format!("invalid native context from {host}: {error}"))
}

/// Whether `host` serves a protocol-4 chain, whose blocks carry only native
/// paid-inference transactions (its nodes refuse any other at submission).
/// `false` when it says it does not, and when it cannot say: the node then
/// still refuses what it could never include.
pub(crate) async fn host_carries_only_native_transactions(state: &AppState, host: &str) -> bool {
    matches!(fetch_context_value(&state.http, host).await, Ok(Some(value)) if context_is_native_only(&value))
}

fn context_is_native_only(value: &Value) -> bool {
    value.get("chain_protocol").and_then(Value::as_u64) == Some(4)
        && value.get("native_only_chain").and_then(Value::as_bool) == Some(true)
}

async fn load_context(http: &reqwest::Client, host: &str) -> Result<ChainContext, String> {
    let value = fetch_context_value(http, host)
        .await?
        .ok_or_else(|| format!("{host} does not expose a compatible native inference context"))?;
    parse_context(&value)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeContextView {
    pub host: String,
    /// False when this app cannot build requests this host's chain admits.
    pub compatible: bool,
    pub reason: Option<String>,
    pub height: Option<u64>,
    pub members: Option<usize>,
    pub executions: Option<usize>,
    pub max_tokens: Option<u32>,
    pub serving: Option<NativeServingView>,
    pub input_kind: Option<String>,
    pub node_version: Option<String>,
    pub app_contract_version: u16,
    pub chain_protocol: Option<u64>,
    pub native_only_chain: Option<bool>,
    pub request_admission_open: bool,
    /// The signed chain context is parseable, so journal and receipt operations remain available.
    pub tracking_available: bool,
}

fn context_view(host: String, parsed: Result<ChainContext, String>) -> NativeContextView {
    let mut view = NativeContextView {
        host,
        compatible: false,
        reason: None,
        height: None,
        members: None,
        executions: None,
        max_tokens: None,
        serving: None,
        input_kind: None,
        node_version: None,
        app_contract_version: INFERENCE_CONTRACT_VERSION,
        chain_protocol: None,
        native_only_chain: None,
        request_admission_open: false,
        tracking_available: false,
    };
    match parsed {
        Err(reason) => view.reason = Some(reason),
        Ok(context) => {
            view.tracking_available = true;
            view.chain_protocol = context.chain_protocol;
            view.native_only_chain = context.native_only_chain;
            view.request_admission_open = context.request_admission_open == Some(true);
            view.height = Some(context.height);
            view.members = Some(context.members);
            view.executions = Some(context.executions.len());
            view.max_tokens = Some(context.max_tokens);
            view.node_version = context.node_version.clone();
            if let Some(reason) = context.request_admission_error() {
                view.reason = Some(reason.to_string());
            } else {
                match input_kind(context.serving.as_ref()) {
                    Ok(kind) => {
                        view.compatible = true;
                        view.input_kind = Some(kind.to_string());
                    }
                    Err(reason) => view.reason = Some(reason),
                }
            }
            view.serving = context.serving;
        }
    }
    view
}

/// The native context of the pinned chain host. `None` when the host does not
/// expose the native-inference context endpoint (the panel stays hidden).
#[tauri::command]
pub async fn native_context(state: State<'_, AppState>) -> CmdResult<Option<NativeContextView>> {
    native_context_inner(&state).await
}

pub(crate) async fn native_context_inner(state: &AppState) -> CmdResult<Option<NativeContextView>> {
    let host = wallet::validate_rpc_origin(&crate::commands::chain_host(state).await)?;
    let Some(value) = fetch_context_value(&state.http, &host).await? else {
        return Ok(None);
    };
    Ok(Some(context_view(host, parse_context(&value))))
}

// ── Preparing the signed input ────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedInput {
    pub input_hex: String,
    pub input_hash: String,
    pub input_kind: String,
    pub token_count: Option<usize>,
    pub byte_len: usize,
    /// What this chain's executor does with the input, in words.
    pub note: String,
}

/// Check that a node's tokenize answer is internally consistent and within
/// the chain's input limit: `input_hex` must be exactly the little-endian
/// encoding of `tokens`.
fn checked_token_input(value: &Value, max_input_bytes: usize) -> Result<Vec<u8>, String> {
    let tokens = value
        .get("tokens")
        .and_then(Value::as_array)
        .ok_or_else(|| "tokenize response has no tokens".to_string())?
        .iter()
        .map(|token| {
            token
                .as_u64()
                .and_then(|token| u32::try_from(token).ok())
                .ok_or_else(|| "tokenize response has a non-u32 token".to_string())
        })
        .collect::<Result<Vec<u32>, String>>()?;
    let input: Vec<u8> = tokens
        .iter()
        .flat_map(|token| token.to_le_bytes())
        .collect();
    let claimed = value
        .get("input_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| "tokenize response has no input_hex".to_string())?;
    if hex::decode(claimed).ok().as_deref() != Some(input.as_slice()) {
        return Err("tokenize response input_hex does not encode its tokens".into());
    }
    if input.is_empty() {
        return Err("the prompt tokenized to nothing".into());
    }
    if input.len() > max_input_bytes {
        return Err(format!(
            "the prompt is {} tokens; the chain's input limit is {}",
            tokens.len(),
            max_input_bytes / 4
        ));
    }
    Ok(input)
}

async fn prepare_input(
    http: &reqwest::Client,
    host: &str,
    context: &ChainContext,
    prompt: &str,
) -> Result<PreparedInput, String> {
    if prompt.trim().is_empty() {
        return Err("enter a prompt".into());
    }
    let kind = input_kind(context.serving.as_ref())?;
    let (input, token_count, note) = if kind == INPUT_CANONICAL_TOKENS {
        let response = http
            .post(format!("{host}/native-inference/tokenize"))
            .json(&json!({ "text": prompt }))
            // Tokenizing a long prompt can take seconds on the node; the
            // client's default timeout is for quick reads.
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| format!("could not reach {host} to tokenize: {error}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "{host} refused to tokenize the prompt (HTTP {status}): {}",
                body.trim()
            ));
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|error| format!("invalid tokenize response from {host}: {error}"))?;
        let input = checked_token_input(&value, context.max_input_bytes)?;
        let count = input.len() / 4;
        (
            input,
            Some(count),
            "Tokenized by the host with the tokenizer its canonical executor loads. You sign \
             these token ids; the validators run exactly them."
                .to_string(),
        )
    } else {
        let input = prompt.as_bytes().to_vec();
        if input.len() > context.max_input_bytes {
            return Err(format!(
                "the prompt is {} bytes; the chain's input limit is {}",
                input.len(),
                context.max_input_bytes
            ));
        }
        (
            input,
            None,
            "This chain runs the deterministic TEST executor. It ignores the prompt and returns \
             protocol-test tokens, not an answer. Payment and settlement on this chain are real."
                .to_string(),
        )
    };
    Ok(PreparedInput {
        input_hash: hash_bytes(&input).to_hex(),
        byte_len: input.len(),
        input_hex: hex::encode(&input),
        input_kind: kind.to_string(),
        token_count,
        note,
    })
}

/// Turn a prompt into the exact input bytes a request would sign.
#[tauri::command]
pub async fn native_prepare(
    state: State<'_, AppState>,
    prompt: String,
) -> CmdResult<PreparedInput> {
    native_prepare_inner(&state, &prompt).await
}

pub(crate) async fn native_prepare_inner(
    state: &AppState,
    prompt: &str,
) -> CmdResult<PreparedInput> {
    let prompt = prompt.to_string();
    let host = wallet::validate_rpc_origin(&crate::commands::chain_host(state).await)?;
    let context = load_context(&state.http, &host).await?;
    if let Some(reason) = context.request_admission_error() {
        return Err(reason.into());
    }
    prepare_input(&state.http, &host, &context, &prompt).await
}

// ── Building and signing ──────────────────────────────────────────────────

/// Choices the user made for one request, in base units and blocks.
#[derive(Clone, Copy, Debug)]
struct RequestTerms {
    max_tokens: u32,
    execution_price: u64,
    reserved_max_payment: u64,
    expiry_blocks: u64,
}

fn check_terms(terms: &RequestTerms, context: &ChainContext, kind: &str) -> Result<(), String> {
    if terms.max_tokens == 0 || terms.max_tokens > context.max_tokens {
        return Err(format!(
            "max tokens must be between 1 and {}",
            context.max_tokens
        ));
    }
    // The test executor always returns two tokens; a smaller bound could
    // never be certified and would only end in a refund.
    if kind == INPUT_TEST_EXECUTOR_BYTES && terms.max_tokens < 2 {
        return Err("the test executor returns two tokens; allow at least 2".into());
    }
    if terms.execution_price == 0 {
        return Err("the price must be greater than zero".into());
    }
    if terms.reserved_max_payment < terms.execution_price {
        return Err("the reservation must cover the price".into());
    }
    if !(MIN_EXPIRY_BLOCKS..=MAX_EXPIRY_BLOCKS).contains(&terms.expiry_blocks) {
        return Err(format!(
            "expiry must be between {MIN_EXPIRY_BLOCKS} and {MAX_EXPIRY_BLOCKS} blocks"
        ));
    }
    Ok(())
}

/// A canonical request must fit the host executor's KV budget: 1 BOS +
/// prompt tokens + `max_tokens` positions. Otherwise that executor refuses it
/// and never votes, and the request can only expire and refund.
fn check_positions(prompt_tokens: u64, max_tokens: u32, limit: Option<u64>) -> Result<(), String> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let needed = 1 + prompt_tokens + u64::from(max_tokens);
    if needed > limit {
        return Err(format!(
            "this request needs {needed} positions (1 + {prompt_tokens} prompt tokens + \
             {max_tokens}), and the host's executor holds at most {limit}; lower max tokens or \
             shorten the prompt"
        ));
    }
    Ok(())
}

/// Build and sign one native request transaction. Pure: no I/O.
#[allow(clippy::too_many_arguments)]
fn build_request_tx(
    keypair: &KeyPair,
    requester: Hash256,
    context: &ChainContext,
    input: Vec<u8>,
    nonce: u64,
    terms: &RequestTerms,
    transaction_domain: Option<Hash256>,
) -> Result<(Transaction, InferenceJob), String> {
    let execution = context.executions[0];
    let expires_at = context
        .height
        .checked_add(terms.expiry_blocks)
        .ok_or_else(|| "expiry height overflows".to_string())?;
    let max_output_bytes = terms
        .max_tokens
        .checked_mul(4)
        .ok_or_else(|| "max tokens is out of range".to_string())?
        .min(context.max_output_bytes);
    let job = InferenceJob {
        version: INFERENCE_CONTRACT_VERSION,
        domain: context.domain,
        requester,
        nonce,
        model_hash: execution.model,
        profile_hash: execution.profile,
        input_hash: hash_bytes(&input),
        generation_hash: execution.generation,
        assignment_hash: execution.assignment,
        max_tokens: terms.max_tokens,
        max_output_bytes,
        execution_price: terms.execution_price,
        reserved_max_payment: terms.reserved_max_payment,
        expires_at,
    };
    let request = InferenceRequest::sign(job.clone(), keypair)
        .map_err(|error| format!("could not sign the request: {error}"))?;
    let body = TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
        request,
        input_blob: input,
    });
    let mut tx = native_tx(requester, nonce, body, gas_costs::NATIVE_INFERENCE_REQUEST);
    wallet::sign_for_domain(&mut tx, keypair, transaction_domain, "native request")?;
    Ok((tx, job))
}

fn native_tx(from: Hash256, nonce: u64, body: TxBody, gas_limit: u64) -> Transaction {
    Transaction {
        tx_type: body.tx_type(),
        from,
        nonce,
        body,
        // Native transactions carry no fee: the price is signed in the job.
        fee: 0,
        gas_limit,
        hash: Hash256::ZERO,
        signature: Signature::null(),
        sig_verified: false,
    }
}

// ── The journal: signed transactions survive a restart ───────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Request,
    Refund,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct JournalRecord {
    kind: RecordKind,
    request_id: String,
    tx_hash: String,
    account: String,
    /// The chain (its genesis hash): records are matched by chain, not by
    /// which host served them, so a change of host never hides an open
    /// record or lets a second transaction take its nonce.
    #[serde(default)]
    chain: String,
    host: String,
    nonce: u64,
    /// For a request, its signed expiry. For a refund, how long it holds the
    /// account nonce if it is never mined.
    expires_at: u64,
    created_at_ms: i64,
    #[serde(default)]
    input_kind: String,
    #[serde(default)]
    prompt_preview: String,
    #[serde(default)]
    execution_price: u64,
    #[serde(default)]
    reserved_max_payment: u64,
    /// A definitive refusal at submission: these bytes will never be admitted.
    #[serde(default)]
    refused: Option<String>,
    /// Kept only while the transaction can still be admitted, so a restart
    /// resubmits the identical signed bytes instead of signing again.
    #[serde(default)]
    signed_tx: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Journal {
    records: Vec<JournalRecord>,
}

impl Journal {
    fn path(dir: &Path) -> PathBuf {
        dir.join(JOURNAL_FILE)
    }

    /// The journal, or why it cannot be trusted. An unreadable or oversized
    /// journal is moved aside and signing stays paused while it is there:
    /// starting from an empty one would forget signed transactions that may
    /// still be admitted, and let a second one take their nonce.
    fn load(dir: &Path) -> Result<Journal, String> {
        let path = Self::path(dir);
        let aside = path.with_extension("json.unreadable");
        if aside.exists() {
            return Err(format!(
                "the native request journal could not be read and was kept at {}; inspect it \
                 (it lists signed requests that may still be admitted) and remove it to resume",
                aside.display()
            ));
        }
        // Only a journal that is verifiably absent starts empty. Any other
        // error (permissions, I/O, a data path that is not a directory) would
        // forget signed transactions that may still be admitted.
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Journal::default());
            }
            Err(error) => {
                return Err(format!(
                    "the native request journal at {} could not be inspected ({error}); signing \
                     stays paused until it can be read",
                    path.display()
                ));
            }
        };
        let parsed = if metadata.len() > JOURNAL_MAX_BYTES {
            Err(format!("it is larger than {JOURNAL_MAX_BYTES} bytes"))
        } else {
            std::fs::read(&path)
                .map_err(|error| error.to_string())
                .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
        };
        parsed.map_err(|error| {
            // Keep the bytes for inspection; never overwrite them.
            let _ = std::fs::rename(&path, &aside);
            tracing::error!(%error, aside = %aside.display(), "native request journal unreadable");
            format!(
                "the native request journal could not be read ({error}) and was kept at {}; \
                 inspect it and remove it to resume",
                aside.display()
            )
        })
    }

    fn save(&self, dir: &Path) -> Result<(), String> {
        let path = Self::path(dir);
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        // Owner-only: the journal holds prompt previews. A temporary left by a
        // crash is replaced, never reused, so it cannot carry wider
        // permissions into the journal.
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
        // The rename itself must survive a crash: a signed request that was
        // submitted but fell out of the journal could never be refunded from
        // this app.
        arc_crypto::secret_file::sync_parent_directory(&path).map_err(|error| error.to_string())
    }

    /// Drop signed bytes that can no longer be admitted on `chain`: the
    /// account has used the nonce (by this transaction or another), or the
    /// transaction's horizon has passed. Returns whether anything changed.
    fn settle(&mut self, chain: &str, account: &str, account_nonce: u64, height: u64) -> bool {
        let mut changed = false;
        for record in &mut self.records {
            if record.chain == chain
                && record.account == account
                && record.signed_tx.is_some()
                && (account_nonce > record.nonce || height >= record.expires_at)
            {
                record.signed_tx = None;
                changed = true;
            }
        }
        changed
    }

    /// A signed transaction of `account` on `chain` that can still take the
    /// account's next nonce, whichever host it went through. Signing another
    /// would compete for it. A record written before records named their
    /// chain (`chain` empty) could belong to any chain, so it holds the nonce
    /// on every chain; no chain's nonce or height can settle it.
    fn open_for(&self, chain: &str, account: &str) -> Option<&JournalRecord> {
        self.records.iter().find(|record| {
            (record.chain == chain || record.chain.is_empty())
                && record.account == account
                && record.signed_tx.is_some()
        })
    }

    fn push(&mut self, record: JournalRecord) {
        self.records.push(record);
        while self.records.len() > JOURNAL_MAX_RECORDS {
            match self
                .records
                .iter()
                .position(|record| record.signed_tx.is_none())
            {
                Some(index) => {
                    self.records.remove(index);
                }
                None => break,
            }
        }
    }

    fn find_mut(&mut self, tx_hash: &str) -> Option<&mut JournalRecord> {
        self.records
            .iter_mut()
            .rev()
            .find(|record| record.tx_hash == tx_hash)
    }
}

/// Why a record from before records named their chain blocks signing, and
/// what the user can do about it.
fn unattributed_hold(open: &JournalRecord, dir: &Path) -> String {
    format!(
        "a transaction this wallet signed before the journal recorded chains (nonce {}, via {}) \
         may still be admitted on any chain, so signing waits; once its receipt shows it \
         settled, remove its record from {}",
        open.nonce,
        open.host,
        Journal::path(dir).display()
    )
}

// ── Submitting ────────────────────────────────────────────────────────────

/// What happened to one signed native transaction at `/tx/submit_signed`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSubmitResult {
    pub kind: RecordKind,
    pub request_id: String,
    pub tx_hash: String,
    pub nonce: u64,
    pub expires_at: u64,
    /// The node acknowledged exactly this transaction as pending.
    pub accepted: bool,
    /// The node's refusal, verbatim, when it refused.
    pub http_status: Option<u16>,
    pub body: Option<String>,
    /// Set when the node could not be reached: delivery is unknown, and the
    /// signed bytes stay journaled for resubmission.
    pub network_error: Option<String>,
}

enum SubmitOutcome {
    Accepted,
    Refused { status: u16, body: String },
    Unreachable(String),
}

impl SubmitOutcome {
    /// A refusal after which these signed bytes can never be admitted: the
    /// node's own validation refusal (400, 413, 422) of a first delivery.
    /// Anything else - busy, rate-limited, "already have it", a proxy's 5xx,
    /// a 404 from something in between - leaves them resubmittable, and so
    /// does a nonce the account already used: that may have been this very
    /// transaction, and its receipt says. Callers apply this only to the
    /// first delivery; a later copy's refusal says nothing about an earlier
    /// copy that may already be in a mempool.
    fn is_definitive_refusal(&self) -> bool {
        match self {
            SubmitOutcome::Refused { status, body } => {
                if !matches!(*status, 400 | 413 | 422) {
                    return false;
                }
                match nonce_refusal(body) {
                    Some((expected, got)) => got > expected,
                    None => true,
                }
            }
            _ => false,
        }
    }
}

/// `(expected, got)` from the node's "invalid nonce: expected E, got G".
fn nonce_refusal(body: &str) -> Option<(u64, u64)> {
    let rest = &body[body.find("invalid nonce: expected ")? + "invalid nonce: expected ".len()..];
    let (expected, rest) = rest.split_once(", got ")?;
    let got: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((expected.trim().parse().ok()?, got.parse().ok()?))
}

async fn post_signed(http: &reqwest::Client, host: &str, tx: &Transaction) -> SubmitOutcome {
    let response = match http
        .post(format!("{host}/tx/submit_signed"))
        .json(tx)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return SubmitOutcome::Unreachable(error.to_string()),
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return SubmitOutcome::Refused {
            status: status.as_u16(),
            body: body.trim().to_string(),
        };
    }
    let acknowledged = serde_json::from_str::<Value>(&body)
        .ok()
        .is_some_and(|value| {
            value.get("status").and_then(Value::as_str) == Some("pending")
                && value
                    .get("tx_hash")
                    .and_then(Value::as_str)
                    .is_some_and(|hash| strip_0x(hash) == tx.hash.to_hex())
        });
    if acknowledged {
        SubmitOutcome::Accepted
    } else {
        // A 2xx that does not name this transaction as pending proves
        // nothing either way: it may have been accepted. Delivery unknown.
        SubmitOutcome::Unreachable(format!(
            "{host} answered without acknowledging this exact transaction as pending"
        ))
    }
}

fn result_for(record: &JournalRecord, outcome: &SubmitOutcome) -> NativeSubmitResult {
    let (accepted, http_status, body, network_error) = match outcome {
        SubmitOutcome::Accepted => (true, None, None, None),
        SubmitOutcome::Refused { status, body } => (false, Some(*status), Some(body.clone()), None),
        SubmitOutcome::Unreachable(error) => (false, None, None, Some(error.clone())),
    };
    NativeSubmitResult {
        kind: record.kind,
        request_id: record.request_id.clone(),
        tx_hash: record.tx_hash.clone(),
        nonce: record.nonce,
        expires_at: record.expires_at,
        accepted,
        http_status,
        body,
        network_error,
    }
}

async fn chain_height(http: &reqwest::Client, host: &str) -> Result<u64, String> {
    let value: Value = http
        .get(format!("{host}/health"))
        .send()
        .await
        .map_err(|error| format!("could not reach {host}: {error}"))?
        .json()
        .await
        .map_err(|error| format!("invalid /health from {host}: {error}"))?;
    value
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{host} did not report an exact height"))
}

/// The chain `host` serves, as its genesis hash: the key journal records are
/// matched by. Read without the full compatibility check, so a refund stays
/// possible even when app and node disagree about building new requests.
async fn chain_identity(http: &reqwest::Client, host: &str) -> Result<String, String> {
    let value = fetch_context_value(http, host)
        .await?
        .ok_or_else(|| format!("{host} is not a protocol-4 chain"))?;
    Ok(hash_field(&value, "chain_genesis")?.to_hex())
}

async fn data_dir(state: &AppState) -> PathBuf {
    state.data_dir.lock().await.clone()
}

/// Take the cross-process journal lock (`JOURNAL_LOCK_FILE`). `wallet_write`
/// serializes signing inside this process only. The returned file holds the
/// lock until it is dropped; callers keep it from the journal load through the
/// save and the submission.
async fn lock_journal(dir: &Path) -> Result<std::fs::File, String> {
    let lock_path = dir.join(JOURNAL_LOCK_FILE);
    tokio::task::spawn_blocking(move || {
        match std::fs::symlink_metadata(&lock_path) {
            Ok(metadata)
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() =>
            {
                return Err(format!(
                    "refusing a native request journal lock that is not a regular file: {}",
                    lock_path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect {}: {}", lock_path.display(), error)),
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .map_err(|error| format!("open {}: {}", lock_path.display(), error))?;
        file.lock_exclusive()
            .map_err(|error| format!("lock {}: {}", lock_path.display(), error))?;
        Ok(file)
    })
    .await
    .map_err(|error| format!("the native request journal lock could not be taken: {error}"))?
}

/// The wallet's address, then (only when signing) its key.
async fn wallet_address(state: &AppState) -> Result<String, String> {
    let store = state.store.lock().await;
    store
        .identity
        .as_ref()
        .map(|identity| strip_0x(&identity.address))
        .ok_or_else(|| "no identity".to_string())
}

async fn wallet_key(state: &AppState) -> Result<(Hash256, KeyPair), String> {
    let store = state.store.lock().await;
    let identity = store
        .identity
        .as_ref()
        .ok_or_else(|| "no identity".to_string())?;
    wallet::wallet_keypair(identity)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn preview(prompt: &str) -> String {
    prompt.chars().take(PROMPT_PREVIEW_CHARS).collect()
}

/// Record `record` durably, submit its transaction, and record the outcome.
/// Nothing is submitted unless the signed bytes were journaled first.
async fn journal_and_submit(
    state: &AppState,
    host: &str,
    mut journal: Journal,
    record: JournalRecord,
    tx: &Transaction,
) -> Result<NativeSubmitResult, String> {
    let dir = data_dir(state).await;
    let tx_hash = record.tx_hash.clone();
    journal.push(record);
    journal.save(&dir).map_err(|error| {
        format!("could not record the signed transaction durably, so it was not submitted: {error}")
    })?;
    let outcome = post_signed(&state.http, host, tx).await;
    let record = journal
        .find_mut(&tx_hash)
        .ok_or_else(|| "journal lost the record it just wrote".to_string())?;
    if let SubmitOutcome::Refused { body, .. } = &outcome {
        if outcome.is_definitive_refusal() {
            record.signed_tx = None;
            record.refused = Some(body.clone());
        }
    }
    let result = result_for(record, &outcome);
    if let Err(error) = journal.save(&dir) {
        // The submission already happened; a stale journal only means a
        // restart may resubmit bytes the node will refuse again.
        tracing::error!(%error, "could not record a native submission outcome");
    }
    Ok(result)
}

/// Sign and submit one native paid request.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn native_submit(
    state: State<'_, AppState>,
    input_hex: String,
    input_kind: String,
    prompt_preview: String,
    max_tokens: u32,
    price_arc: String,
    reserve_arc: String,
    expiry_blocks: u64,
) -> CmdResult<NativeSubmitResult> {
    native_submit_inner(
        &state,
        NativeSubmitRequest {
            input_hex,
            input_kind,
            prompt_preview,
            max_tokens,
            price_arc,
            reserve_arc,
            expiry_blocks,
        },
    )
    .await
}

/// The WebView's request, as `native_submit` receives it.
#[derive(Clone, Debug)]
pub(crate) struct NativeSubmitRequest {
    pub input_hex: String,
    pub input_kind: String,
    pub prompt_preview: String,
    pub max_tokens: u32,
    pub price_arc: String,
    pub reserve_arc: String,
    pub expiry_blocks: u64,
}

pub(crate) async fn native_submit_inner(
    state: &AppState,
    request: NativeSubmitRequest,
) -> CmdResult<NativeSubmitResult> {
    let NativeSubmitRequest {
        input_hex,
        input_kind,
        prompt_preview,
        max_tokens,
        price_arc,
        reserve_arc,
        expiry_blocks,
    } = request;
    let terms = RequestTerms {
        max_tokens,
        execution_price: wallet::parse_arc_amount(&price_arc)?,
        reserved_max_payment: wallet::parse_arc_amount(&reserve_arc)?,
        expiry_blocks,
    };
    let input = hex::decode(input_hex.trim())
        .map_err(|_| "the prepared input is not hexadecimal".to_string())?;

    // One signer at a time, shared with transfers: two clicks must never
    // sign the same account nonce.
    let _write_guard = state.wallet_write.lock().await;
    let address = wallet_address(state).await?;
    let host = wallet::validate_rpc_origin(&crate::commands::chain_host(state).await)?;
    let context = load_context(&state.http, &host).await?;
    if let Some(reason) = context.request_admission_error() {
        return Err(reason.into());
    }
    let kind = input_kind_checked(&context, &input_kind, &input)?;
    check_terms(&terms, &context, kind)?;
    if kind == INPUT_CANONICAL_TOKENS {
        check_positions(
            (input.len() / 4) as u64,
            terms.max_tokens,
            context
                .serving
                .as_ref()
                .and_then(|serving| serving.max_positions),
        )?;
    }
    let chain = context.domain.chain_genesis.to_hex();

    let dir = data_dir(state).await;
    let _journal_lock = lock_journal(&dir).await?;
    // Read the account under the lock: a nonce read before it could already
    // have been used by another app instance on this data directory.
    let account = rpc_client::fetch_balance(&state.http, &host, &address).await?;
    let balance = account
        .balance_base
        .parse::<u64>()
        .map_err(|_| "the host returned an invalid wallet balance".to_string())?;
    let mut journal = Journal::load(&dir)?;
    if journal.settle(&chain, &address, account.nonce, context.height) {
        journal.save(&dir)?;
    }
    if let Some(open) = journal.open_for(&chain, &address) {
        if open.chain.is_empty() {
            return Err(unattributed_hold(open, &dir));
        }
        return Err(format!(
            "an earlier transaction from this wallet (nonce {}) is not in a block yet; it is \
             admitted or expires by block {}, and then this request can be signed",
            open.nonce, open.expires_at
        ));
    }
    if balance < terms.reserved_max_payment {
        return Err(format!(
            "insufficient balance: available {} ARC; this request reserves {} ARC until it settles",
            account.balance_arc,
            wallet::format_arc_amount(terms.reserved_max_payment)
        ));
    }
    let domain = rpc_client::transaction_signing_domain(&state.http, &host).await?;
    let (requester, keypair) = wallet_key(state).await?;
    if !requester.to_hex().eq_ignore_ascii_case(&address) {
        return Err("stored wallet address changed while signing".into());
    }
    let (tx, job) = build_request_tx(
        &keypair,
        requester,
        &context,
        input,
        account.nonce,
        &terms,
        domain,
    )?;
    drop(keypair);
    let record = JournalRecord {
        kind: RecordKind::Request,
        request_id: job.request_id().to_hex(),
        tx_hash: tx.hash.to_hex(),
        account: address,
        chain: chain.clone(),
        host: host.clone(),
        nonce: job.nonce,
        expires_at: job.expires_at,
        created_at_ms: now_ms(),
        input_kind: kind.to_string(),
        prompt_preview: preview(&prompt_preview),
        execution_price: job.execution_price,
        reserved_max_payment: job.reserved_max_payment,
        refused: None,
        signed_tx: Some(serde_json::to_value(&tx).map_err(|error| error.to_string())?),
    };
    journal_and_submit(state, &host, journal, record, &tx).await
}

/// The input kind the WebView prepared must still be what this host's
/// executor reads, and the bytes must fit it.
fn input_kind_checked(
    context: &ChainContext,
    prepared_kind: &str,
    input: &[u8],
) -> Result<&'static str, String> {
    let kind = input_kind(context.serving.as_ref())?;
    if kind != prepared_kind {
        return Err(
            "the host's executor changed since the prompt was prepared; prepare it again".into(),
        );
    }
    if input.is_empty() || input.len() > context.max_input_bytes {
        return Err("the prepared input is empty or over the chain's input limit".into());
    }
    if kind == INPUT_CANONICAL_TOKENS && input.len() % 4 != 0 {
        return Err("the prepared input is not whole token ids".into());
    }
    Ok(kind)
}

// ── Following a request ───────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeReceiptView {
    pub found: bool,
    pub height: u64,
    /// The node's receipt, verbatim, when it has one.
    pub receipt: Option<Value>,
}

/// The chain's receipt for one request, with the height it was read at.
#[tauri::command]
pub async fn native_receipt(
    state: State<'_, AppState>,
    request_id: String,
) -> CmdResult<NativeReceiptView> {
    native_receipt_inner(&state, &request_id).await
}

pub(crate) async fn native_receipt_inner(
    state: &AppState,
    request_id: &str,
) -> CmdResult<NativeReceiptView> {
    let id = Hash256::from_hex(&strip_0x(request_id))
        .map_err(|_| "request id must be a 32-byte hash".to_string())?;
    let host = wallet::validate_rpc_origin(&crate::commands::chain_host(state).await)?;
    let height = chain_height(&state.http, &host).await?;
    let response = state
        .http
        .get(format!("{host}/native-inference/receipt/{}", id.to_hex()))
        .send()
        .await
        .map_err(|error| format!("could not reach {host}: {error}"))?;
    if response.status().as_u16() == 404 {
        return Ok(NativeReceiptView {
            found: false,
            height,
            receipt: None,
        });
    }
    if !response.status().is_success() {
        return Err(format!(
            "{host} could not read the receipt (HTTP {})",
            response.status()
        ));
    }
    let receipt: Value = response
        .json()
        .await
        .map_err(|error| format!("invalid receipt from {host}: {error}"))?;
    if receipt
        .get("request_id")
        .and_then(Value::as_str)
        .map(strip_0x)
        .as_deref()
        != Some(id.to_hex().as_str())
    {
        return Err(format!("{host} returned a receipt for a different request"));
    }
    Ok(NativeReceiptView {
        found: true,
        height,
        receipt: Some(receipt),
    })
}

/// Claim back the reservation of an admitted request that expired without a
/// certificate. Anyone may; the chain returns it to the requester.
#[tauri::command]
pub async fn native_refund(
    state: State<'_, AppState>,
    request_id: String,
) -> CmdResult<NativeSubmitResult> {
    native_refund_inner(&state, &request_id).await
}

pub(crate) async fn native_refund_inner(
    state: &AppState,
    request_id: &str,
) -> CmdResult<NativeSubmitResult> {
    let id = Hash256::from_hex(&strip_0x(request_id))
        .map_err(|_| "request id must be a 32-byte hash".to_string())?;
    let _write_guard = state.wallet_write.lock().await;
    let address = wallet_address(state).await?;
    let host = wallet::validate_rpc_origin(&crate::commands::chain_host(state).await)?;
    let chain = chain_identity(&state.http, &host).await?;
    let height = chain_height(&state.http, &host).await?;
    let receipt = state
        .http
        .get(format!("{host}/native-inference/receipt/{}", id.to_hex()))
        .send()
        .await
        .map_err(|error| format!("could not reach {host}: {error}"))?;
    if receipt.status().as_u16() == 404 {
        return Err(
            "the chain has no admitted request with this id, so nothing is reserved".into(),
        );
    }
    if !receipt.status().is_success() {
        return Err(format!(
            "{host} could not read the request's receipt (HTTP {}); nothing was signed",
            receipt.status()
        ));
    }
    let receipt: Value = receipt
        .json()
        .await
        .map_err(|error| format!("invalid receipt from {host}: {error}"))?;
    match receipt.get("observed_status").and_then(Value::as_str) {
        Some("Pending") => {}
        Some("Refunded") => return Err("this request is already refunded".into()),
        Some("Finalized") => {
            return Err("this request finalized; there is nothing to refund".into())
        }
        other => return Err(format!("unrecognised request status {other:?}")),
    }
    let expires_at = receipt
        .get("expires_at")
        .and_then(Value::as_u64)
        .ok_or_else(|| "the receipt does not state its expiry; update the node".to_string())?;
    // The chain refunds at the first block whose height is at or past the
    // expiry; the next block is `height + 1`.
    if height.saturating_add(1) < expires_at {
        return Err(format!(
            "not refundable yet: the request expires at block {expires_at} (now {height})"
        ));
    }
    let dir = data_dir(state).await;
    let _journal_lock = lock_journal(&dir).await?;
    // Under the lock, as in native_submit_inner.
    let account = rpc_client::fetch_balance(&state.http, &host, &address).await?;
    let mut journal = Journal::load(&dir)?;
    if journal.settle(&chain, &address, account.nonce, height) {
        journal.save(&dir)?;
    }
    if let Some(open) = journal.open_for(&chain, &address) {
        if open.chain.is_empty() {
            return Err(unattributed_hold(open, &dir));
        }
        return Err(format!(
            "an earlier transaction from this wallet (nonce {}) is not in a block yet; claim \
             the refund once it is",
            open.nonce
        ));
    }
    let domain = rpc_client::transaction_signing_domain(&state.http, &host).await?;
    let (from, keypair) = wallet_key(state).await?;
    let mut tx = native_tx(
        from,
        account.nonce,
        TxBody::NativeInferenceRefund(NativeInferenceRefundBody { request_id: id.0 }),
        gas_costs::NATIVE_INFERENCE_REFUND,
    );
    wallet::sign_for_domain(&mut tx, &keypair, domain, "refund")?;
    drop(keypair);
    let record = JournalRecord {
        kind: RecordKind::Refund,
        request_id: id.to_hex(),
        tx_hash: tx.hash.to_hex(),
        account: address,
        chain: chain.clone(),
        host: host.clone(),
        nonce: account.nonce,
        expires_at: height.saturating_add(REFUND_RESERVATION_BLOCKS),
        created_at_ms: now_ms(),
        input_kind: String::new(),
        prompt_preview: String::new(),
        execution_price: 0,
        reserved_max_payment: receipt
            .get("reserved_max_payment")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        refused: None,
        signed_tx: Some(serde_json::to_value(&tx).map_err(|error| error.to_string())?),
    };
    journal_and_submit(state, &host, journal, record, &tx).await
}

/// Resubmit the identical signed bytes of a journaled transaction, e.g.
/// after a restart or a transient refusal. Never signs anything new.
#[tauri::command]
pub async fn native_resubmit(
    state: State<'_, AppState>,
    tx_hash: String,
) -> CmdResult<NativeSubmitResult> {
    native_resubmit_inner(&state, &tx_hash).await
}

pub(crate) async fn native_resubmit_inner(
    state: &AppState,
    tx_hash: &str,
) -> CmdResult<NativeSubmitResult> {
    let tx_hash = strip_0x(tx_hash);
    let _write_guard = state.wallet_write.lock().await;
    let address = wallet_address(state).await?;
    let host = wallet::validate_rpc_origin(&crate::commands::chain_host(state).await)?;
    let chain = chain_identity(&state.http, &host).await?;
    let height = chain_height(&state.http, &host).await?;
    let dir = data_dir(state).await;
    let _journal_lock = lock_journal(&dir).await?;
    // Under the lock, as in native_submit_inner.
    let account = rpc_client::fetch_balance(&state.http, &host, &address).await?;
    let mut journal = Journal::load(&dir)?;
    if journal.settle(&chain, &address, account.nonce, height) {
        journal.save(&dir)?;
    }
    let record = journal
        .records
        .iter()
        .rev()
        .find(|record| record.tx_hash == tx_hash && record.chain == chain)
        .cloned()
        .ok_or_else(|| "no journaled transaction with this hash on this chain".to_string())?;
    let Some(signed) = record.signed_tx.clone() else {
        return Err(
            "this transaction can no longer be admitted (its nonce was used or it expired); \
             its receipt decides the outcome"
                .into(),
        );
    };
    let tx: Transaction = serde_json::from_value(signed)
        .map_err(|error| format!("journaled transaction is unreadable: {error}"))?;
    if tx.hash.to_hex() != record.tx_hash {
        return Err("journaled transaction does not match its recorded hash".into());
    }
    // A resubmission's refusal never releases the signed bytes: the first
    // copy may already be in a mempool. The nonce or the expiry decides
    // (`Journal::settle`).
    let outcome = post_signed(&state.http, &host, &tx).await;
    Ok(result_for(&record, &outcome))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeJournalEntry {
    pub kind: RecordKind,
    pub request_id: String,
    pub tx_hash: String,
    pub nonce: u64,
    pub expires_at: u64,
    pub created_at_ms: i64,
    pub input_kind: String,
    pub prompt_preview: String,
    pub execution_price: u64,
    pub reserved_max_payment: u64,
    pub refused: Option<String>,
    /// The signed bytes are still held and can be resubmitted.
    pub resubmittable: bool,
}

/// This wallet's journaled native transactions on the pinned host, oldest
/// first, so the WebView can resume following them after a restart.
#[tauri::command]
pub async fn native_journal(state: State<'_, AppState>) -> CmdResult<Vec<NativeJournalEntry>> {
    native_journal_inner(&state).await
}

pub(crate) async fn native_journal_inner(state: &AppState) -> CmdResult<Vec<NativeJournalEntry>> {
    let address = wallet_address(state).await?;
    let host = wallet::validate_rpc_origin(&crate::commands::chain_host(state).await)?;
    let chain = chain_identity(&state.http, &host).await?;
    let dir = data_dir(state).await;
    let mut journal = Journal::load(&dir)?;
    // Best effort, in memory only: say truthfully what can still be
    // resubmitted without writing anything. Only the signing paths, which
    // hold the wallet lock and the journal lock, rewrite the journal; a
    // listing that saved could drop a record one of them had just written.
    // Offline, list what is recorded.
    if let (Ok(height), Ok(account)) = (
        chain_height(&state.http, &host).await,
        rpc_client::fetch_balance(&state.http, &host, &address).await,
    ) {
        journal.settle(&chain, &address, account.nonce, height);
    }
    Ok(journal
        .records
        .iter()
        .filter(|record| record.chain == chain && record.account == address)
        .map(|record| NativeJournalEntry {
            kind: record.kind,
            request_id: record.request_id.clone(),
            tx_hash: record.tx_hash.clone(),
            nonce: record.nonce,
            expires_at: record.expires_at,
            created_at_ms: record.created_at_ms,
            input_kind: record.input_kind.clone(),
            prompt_preview: record.prompt_preview.clone(),
            execution_price: record.execution_price,
            reserved_max_payment: record.reserved_max_payment,
            refused: record.refused.clone(),
            resubmittable: record.signed_tx.is_some(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key(seed: u8) -> KeyPair {
        KeyPair::Ed25519(SigningKey::from_bytes(&[seed; 32]))
    }

    fn context_json() -> Value {
        let members: Vec<ValidatorMember> = (1..=4u8)
            .map(|seed| ValidatorMember::new(key(seed).address(), 1_000))
            .collect();
        let mut sorted = members.clone();
        sorted.sort_by_key(|member| member.address.0);
        let set_hash = validator_set_commitment(&sorted).unwrap();
        let tuple = hash_bytes(b"tuple");
        json!({
            "candidate_protocol": 4,
            "chain_protocol": 4,
            "native_only_chain": true,
            "request_admission_open": true,
            "context_commitment": hash_bytes(b"commitment").to_hex(),
            "chain_genesis": hash_bytes(b"genesis").to_hex(),
            "recovery_epoch": 0,
            "validator_set_hash": set_hash.to_hex(),
            "members": sorted.iter().map(|member| json!({
                "address": member.address.to_hex(), "stake": member.stake,
            })).collect::<Vec<_>>(),
            "allowed_execution_count": 1,
            "allowed_executions": [{
                "model_hash": tuple.to_hex(), "profile_hash": tuple.to_hex(),
                "generation_hash": tuple.to_hex(), "assignment_hash": tuple.to_hex(),
            }],
            "contract_version": INFERENCE_CONTRACT_VERSION,
            "height": 100,
            "limits": {
                "max_tokens": 2048, "max_output_bytes": 16384, "max_input_bytes": 32768,
                "request_gas_limit": gas_costs::NATIVE_INFERENCE_REQUEST,
                "refund_gas_limit": gas_costs::NATIVE_INFERENCE_REFUND,
            },
            "serving": {
                "executor": "deterministic_test", "input_format": "opaque_bytes",
                "tokenizer_profile": null, "tokenize_endpoint": false,
            },
            "node_version": "0.8.0",
        })
    }

    #[test]
    fn a_current_node_context_is_compatible_and_says_how_input_is_built() {
        let view = context_view(
            "http://127.0.0.1:9090".into(),
            parse_context(&context_json()),
        );
        assert!(view.compatible, "{:?}", view.reason);
        assert_eq!(view.input_kind.as_deref(), Some(INPUT_TEST_EXECUTOR_BYTES));
        assert_eq!(view.members, Some(4));
        assert_eq!(view.height, Some(100));
        assert_eq!(view.chain_protocol, Some(4));
        assert!(view.native_only_chain.unwrap());
        assert!(view.request_admission_open);
    }

    #[test]
    fn migrated_v3_can_admit_requests_without_being_classified_native_only() {
        let mut migrated = context_json();
        migrated["chain_protocol"] = json!(3);
        migrated["native_only_chain"] = json!(false);
        let context = parse_context(&migrated).unwrap();
        assert_eq!(context.chain_protocol, Some(3));
        assert_eq!(context.request_admission_error(), None);
        assert!(!context_is_native_only(&migrated));
    }

    #[test]
    fn closed_or_unadvertised_admission_keeps_context_readable_but_blocks_requests() {
        let mut closed = context_json();
        closed["request_admission_open"] = json!(false);
        let parsed = parse_context(&closed).unwrap();
        assert!(parsed
            .request_admission_error()
            .unwrap()
            .contains("not admitting"));
        let view = context_view("h".into(), Ok(parsed));
        assert!(!view.compatible);
        assert!(!view.request_admission_open);
        assert!(view.tracking_available);
        assert!(view.reason.unwrap().contains("not admitting"));

        let mut old = context_json();
        old.as_object_mut()
            .unwrap()
            .remove("request_admission_open");
        let parsed = parse_context(&old).unwrap();
        assert!(parsed
            .request_admission_error()
            .unwrap()
            .contains("update the node"));
        // Receipt/refund paths parse the context without requiring request admission.
        assert_eq!(parsed.height, 100);
    }

    #[test]
    fn native_only_classification_requires_explicit_protocol_four_fields() {
        let mut context = context_json();
        assert!(context_is_native_only(&context));
        context["chain_protocol"] = json!(3);
        assert!(!context_is_native_only(&context));
        context["chain_protocol"] = json!(4);
        context["native_only_chain"] = json!(false);
        assert!(!context_is_native_only(&context));
        context.as_object_mut().unwrap().remove("chain_protocol");
        assert!(!context_is_native_only(&context));
    }

    #[test]
    fn incompatible_nodes_are_named_with_what_to_update() {
        let mut old = context_json();
        old.as_object_mut().unwrap().remove("allowed_executions");
        assert!(parse_context(&old).unwrap_err().contains("update the node"));

        let mut newer = context_json();
        newer["contract_version"] = json!(u64::from(INFERENCE_CONTRACT_VERSION) + 1);
        assert!(parse_context(&newer)
            .unwrap_err()
            .contains("update the app"));

        let mut gas = context_json();
        gas["limits"]["request_gas_limit"] = json!(1);
        assert!(parse_context(&gas)
            .unwrap_err()
            .contains("different releases"));

        let mut members = context_json();
        members["members"][0]["stake"] = json!(1);
        assert!(parse_context(&members)
            .unwrap_err()
            .contains("does not match its validator-set hash"));

        let mut legacy = context_json();
        legacy["candidate_protocol"] = json!(3);
        assert!(parse_context(&legacy).is_err());
    }

    #[test]
    fn a_host_that_cannot_build_input_is_not_offered() {
        let mut none = context_json();
        none["serving"] = Value::Null;
        let view = context_view("h".into(), parse_context(&none));
        assert!(!view.compatible);
        assert!(view.tracking_available);
        assert!(view.reason.unwrap().contains("runs no native executor"));

        let mut untokenized = context_json();
        untokenized["serving"] = json!({
            "executor": "canonical_i8", "input_format": "le_u32_token_ids_without_bos",
            "tokenize_endpoint": false,
        });
        let view = context_view("h".into(), parse_context(&untokenized));
        assert!(!view.compatible);
        assert!(view.reason.unwrap().contains("serves no tokenizer"));
    }

    #[test]
    fn a_built_request_is_exactly_what_the_chain_checks() {
        let context = parse_context(&context_json()).unwrap();
        let requester_key = key(9);
        let requester = requester_key.address();
        let terms = RequestTerms {
            max_tokens: 8,
            execution_price: 10,
            reserved_max_payment: 100,
            expiry_blocks: 600,
        };
        check_terms(&terms, &context, INPUT_TEST_EXECUTOR_BYTES).unwrap();
        let (tx, job) = build_request_tx(
            &requester_key,
            requester,
            &context,
            b"hi".to_vec(),
            7,
            &terms,
            None,
        )
        .unwrap();
        assert_eq!(tx.fee, 0);
        assert_eq!(tx.nonce, 7);
        assert_eq!(tx.from, requester);
        assert_eq!(tx.gas_limit, gas_costs::NATIVE_INFERENCE_REQUEST);
        assert_eq!(tx.hash, tx.compute_hash());
        assert!(tx.verify_signature().is_ok());
        assert_eq!(job.expires_at, 700);
        assert_eq!(job.max_output_bytes, 32);
        assert_eq!(job.domain, context.domain);
        assert_eq!(job.input_hash, hash_bytes(b"hi"));
        let TxBody::NativeInferenceRequest(body) = &tx.body else {
            panic!("not a native request");
        };
        // The same request-signature check the chain runs at admission.
        assert!(body.request.verify_signature().is_ok());
        assert_eq!(body.request.job.request_id(), job.request_id());
        assert_eq!(body.input_blob, b"hi");

        // Signed in the recovery domain when one is active.
        let domain = hash_bytes(b"recovery-domain");
        let (in_domain, _) = build_request_tx(
            &requester_key,
            requester,
            &context,
            b"hi".to_vec(),
            7,
            &terms,
            Some(domain),
        )
        .unwrap();
        assert!(in_domain.verify_signature_in_domain(&domain).is_ok());
    }

    #[test]
    fn terms_the_chain_or_executor_would_refuse_are_refused_before_signing() {
        let context = parse_context(&context_json()).unwrap();
        let good = RequestTerms {
            max_tokens: 8,
            execution_price: 10,
            reserved_max_payment: 10,
            expiry_blocks: 600,
        };
        assert!(check_terms(&good, &context, INPUT_TEST_EXECUTOR_BYTES).is_ok());
        for bad in [
            RequestTerms {
                max_tokens: 0,
                ..good
            },
            RequestTerms {
                max_tokens: 4096,
                ..good
            },
            RequestTerms {
                max_tokens: 1,
                ..good
            },
            RequestTerms {
                execution_price: 0,
                ..good
            },
            RequestTerms {
                reserved_max_payment: 9,
                ..good
            },
            RequestTerms {
                expiry_blocks: 1,
                ..good
            },
            RequestTerms {
                expiry_blocks: MAX_EXPIRY_BLOCKS + 1,
                ..good
            },
        ] {
            assert!(
                check_terms(&bad, &context, INPUT_TEST_EXECUTOR_BYTES).is_err(),
                "{bad:?}"
            );
        }
        // One token is fine for the canonical executor.
        assert!(check_terms(
            &RequestTerms {
                max_tokens: 1,
                ..good
            },
            &context,
            INPUT_CANONICAL_TOKENS
        )
        .is_ok());
    }

    #[test]
    fn a_tokenize_answer_must_encode_its_own_tokens() {
        let good = json!({"tokens": [1, 256], "input_hex": "0100000000010000"});
        assert_eq!(
            checked_token_input(&good, 1024).unwrap(),
            vec![1, 0, 0, 0, 0, 1, 0, 0]
        );
        let lying = json!({"tokens": [1, 256], "input_hex": "0200000000010000"});
        assert!(checked_token_input(&lying, 1024).is_err());
        let empty = json!({"tokens": [], "input_hex": ""});
        assert!(checked_token_input(&empty, 1024).is_err());
        assert!(checked_token_input(&good, 4).is_err());
    }

    fn record(nonce: u64, expires_at: u64, open: bool) -> JournalRecord {
        JournalRecord {
            kind: RecordKind::Request,
            request_id: format!("{nonce:064x}"),
            tx_hash: format!("{:064x}", nonce + 1_000),
            account: "aa".into(),
            // Records are matched by chain; the host is informational.
            chain: "h".into(),
            host: "some-host".into(),
            nonce,
            expires_at,
            created_at_ms: 0,
            input_kind: INPUT_TEST_EXECUTOR_BYTES.into(),
            prompt_preview: String::new(),
            execution_price: 10,
            reserved_max_payment: 100,
            refused: None,
            signed_tx: open.then(|| json!({})),
        }
    }

    #[test]
    fn signed_bytes_are_held_only_while_they_can_still_be_admitted() {
        let mut journal = Journal::default();
        journal.push(record(5, 700, true));
        // Same nonce, before expiry: it blocks signing another.
        assert!(!journal.settle("h", "aa", 5, 100));
        assert_eq!(journal.open_for("h", "aa").map(|r| r.nonce), Some(5));
        // Another chain or account is someone else's nonce.
        assert!(journal.open_for("other", "aa").is_none());
        assert!(journal.open_for("h", "bb").is_none());
        assert!(!journal.settle("other", "aa", 9, 100));
        // The account moved past the nonce: admitted or superseded.
        assert!(journal.settle("h", "aa", 6, 100));
        assert!(journal.open_for("h", "aa").is_none());

        let mut expired = Journal::default();
        expired.push(record(5, 700, true));
        assert!(expired.settle("h", "aa", 5, 700));
        assert!(expired.open_for("h", "aa").is_none());
    }

    #[test]
    fn the_journal_trims_only_what_no_longer_holds_signed_bytes() {
        let mut journal = Journal::default();
        journal.push(record(0, 10, true));
        for nonce in 1..=(JOURNAL_MAX_RECORDS as u64 + 5) {
            journal.push(record(nonce, 10, false));
        }
        assert_eq!(journal.records.len(), JOURNAL_MAX_RECORDS);
        assert!(journal
            .records
            .iter()
            .any(|record| record.signed_tx.is_some()));
    }

    #[test]
    fn the_journal_round_trips_and_sets_aside_unreadable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::default();
        journal.push(record(3, 50, true));
        journal.save(dir.path()).unwrap();
        assert_eq!(Journal::load(dir.path()).unwrap().records, journal.records);

        // Unreadable: moved aside, and signing stays paused (every load
        // refuses) until someone inspects and removes it. Starting empty
        // would forget signed requests that may still be admitted.
        std::fs::write(dir.path().join(JOURNAL_FILE), b"{not json").unwrap();
        let aside = dir.path().join("native-requests.json.unreadable");
        assert!(Journal::load(dir.path()).unwrap_err().contains("inspect"));
        assert!(aside.exists());
        assert!(!dir.path().join(JOURNAL_FILE).exists());
        assert!(
            Journal::load(dir.path()).is_err(),
            "still paused on the next load"
        );
        std::fs::remove_file(&aside).unwrap();
        assert!(Journal::load(dir.path()).unwrap().records.is_empty());

        // Oversized: the same, never overwritten.
        std::fs::write(
            dir.path().join(JOURNAL_FILE),
            vec![b' '; JOURNAL_MAX_BYTES as usize + 1],
        )
        .unwrap();
        assert!(Journal::load(dir.path()).is_err());
        assert!(aside.exists());
    }

    #[test]
    fn a_canonical_request_must_fit_the_executors_positions() {
        assert!(check_positions(100, 8, None).is_ok());
        assert!(check_positions(2_039, 8, Some(2_048)).is_ok());
        let refused = check_positions(2_040, 8, Some(2_048)).unwrap_err();
        assert!(refused.contains("2049 positions"), "{refused}");
        assert!(check_positions(0, 2_048, Some(2_048)).is_err());
    }

    #[test]
    fn only_final_refusals_release_the_signed_bytes() {
        let refused = |status: u16, body: &str| SubmitOutcome::Refused {
            status,
            body: body.into(),
        };
        assert!(refused(400, "insufficient balance").is_definitive_refusal());
        assert!(
            refused(400, "native request input does not match its signed hash")
                .is_definitive_refusal()
        );
        // A used nonce may be this very transaction: keep it for the receipt.
        assert!(!refused(400, "invalid nonce: expected 5, got 4").is_definitive_refusal());
        // A nonce ahead of the account was refused and never entered a mempool.
        assert!(
            refused(400, "execution error: invalid nonce: expected 4, got 5")
                .is_definitive_refusal()
        );
        assert_eq!(
            nonce_refusal("invalid nonce: expected 12, got 9"),
            Some((12, 9))
        );
        assert_eq!(nonce_refusal("insufficient balance"), None);
        assert!(!refused(409, "").is_definitive_refusal());
        assert!(!refused(429, "").is_definitive_refusal());
        assert!(!refused(503, "rebuilding its DAG").is_definitive_refusal());
        // Only the node's own validation refusals are final: a proxy's 5xx
        // or a 404 from something in between may follow a delivery.
        for status in [500, 502, 504, 404, 401] {
            assert!(
                !refused(status, "gateway").is_definitive_refusal(),
                "{status}"
            );
        }
        assert!(refused(413, "too large").is_definitive_refusal());
        assert!(refused(422, "unprocessable").is_definitive_refusal());
        assert!(!SubmitOutcome::Accepted.is_definitive_refusal());
        assert!(!SubmitOutcome::Unreachable("timeout".into()).is_definitive_refusal());
    }

    #[test]
    fn a_host_whose_input_format_is_not_the_nodes_is_not_offered() {
        let mut other = context_json();
        other["serving"]["input_format"] = json!("utf8_text");
        let view = context_view("h".into(), parse_context(&other));
        assert!(!view.compatible);
        assert!(view.reason.unwrap().contains("reads input as 'utf8_text'"));

        let mut canonical = context_json();
        canonical["serving"] = json!({
            "executor": "canonical_i8", "input_format": "le_u32_token_ids_with_bos",
            "tokenize_endpoint": true,
        });
        let view = context_view("h".into(), parse_context(&canonical));
        assert!(!view.compatible);
        assert!(view.reason.unwrap().contains("different releases"));

        // The formats the node's executors state are accepted.
        canonical["serving"]["input_format"] = json!(CANONICAL_INPUT_FORMAT);
        let view = context_view("h".into(), parse_context(&canonical));
        assert!(view.compatible, "{:?}", view.reason);
        assert_eq!(view.input_kind.as_deref(), Some(INPUT_CANONICAL_TOKENS));
    }

    #[test]
    fn a_record_without_a_chain_holds_the_nonce_on_every_chain() {
        let mut legacy = record(5, 700, true);
        legacy.chain = String::new();
        let mut journal = Journal::default();
        journal.push(legacy);
        assert!(journal.open_for("h", "aa").is_some());
        assert!(journal.open_for("other", "aa").is_some());
        // Another account's nonce is still its own.
        assert!(journal.open_for("h", "bb").is_none());
        // No chain's nonce or height can say it settled.
        assert!(!journal.settle("h", "aa", 99, 99_999));
        let open = journal.open_for("other", "aa").unwrap();
        let message = unattributed_hold(open, Path::new("/data"));
        assert!(
            message.contains("before the journal recorded chains"),
            "{message}"
        );
        assert!(message.contains(JOURNAL_FILE), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_that_cannot_be_inspected_pauses_signing() {
        // Not a missing journal: the data path is a file, so inspecting the
        // journal fails with NotADirectory. Starting empty would forget signed
        // transactions that may still be admitted.
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("data");
        std::fs::write(
            &not_a_directory,
            b"a file where the data directory should be",
        )
        .unwrap();
        let error = Journal::load(&not_a_directory).unwrap_err();
        assert!(error.contains("could not be inspected"), "{error}");
        // A journal that is simply absent still starts empty.
        assert!(Journal::load(dir.path()).unwrap().records.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_journal_is_owner_only_even_over_a_stale_temporary() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("native-requests.json.tmp");
        std::fs::write(&stale, b"left by a crash").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut journal = Journal::default();
        journal.push(record(1, 50, true));
        journal.save(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(JOURNAL_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!stale.exists());
        assert_eq!(Journal::load(dir.path()).unwrap().records, journal.records);
    }

    #[tokio::test]
    async fn the_journal_lock_excludes_a_second_holder() {
        use fs2::FileExt as _;
        let dir = tempfile::tempdir().unwrap();
        let held = lock_journal(dir.path()).await.unwrap();
        // A second holder (another app instance on this data directory)
        // cannot take it while it is held...
        let other = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join(JOURNAL_LOCK_FILE))
            .unwrap();
        assert!(other.try_lock_exclusive().is_err());
        // ...and can once it is released.
        drop(held);
        assert!(other.try_lock_exclusive().is_ok());
    }
}

/// The real command cores against a real protocol-4 chain. Ignored by
/// default: it needs a running chain whose genesis funds the phrase's wallet
/// (harness `--fund`). Settle path:
///
///   ARC_WALLET_HOST=http://127.0.0.1:9960 ARC_LIVE_PHRASE="<funded test phrase>" \
///     cargo test --lib native_paid::live_journey -- --ignored --nocapture
///
/// Refund path, on a chain whose voting committee is below quorum (harness
/// `--native-workers 2`): add `ARC_LIVE_EXPECT=refund`.
#[cfg(test)]
mod live_journey {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn state(dir: &Path, identity: crate::types::Identity) -> AppState {
        let store = crate::store::Store {
            identity: Some(identity),
            ..Default::default()
        };
        AppState {
            node: Arc::new(Mutex::new(crate::node_manager::NodeManager::new())),
            store: Arc::new(Mutex::new(store)),
            data_dir: Arc::new(Mutex::new(dir.to_path_buf())),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap(),
            tier1_routes: Arc::new(Mutex::new(HashMap::new())),
            community_receipt_routes: Arc::new(Mutex::new(HashMap::new())),
            community_chain_host: Arc::new(Mutex::new(None)),
            community_inference_write: Arc::new(Mutex::new(())),
            chain_host: Arc::new(Mutex::new(None)),
            wallet_write: Arc::new(Mutex::new(())),
            has_tray: Arc::new(AtomicBool::new(false)),
            data_migration_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Poll the receipt until `done` holds, at most `polls` seconds.
    async fn wait(
        state: &AppState,
        request_id: &str,
        polls: u32,
        done: impl Fn(&NativeReceiptView) -> bool,
    ) -> NativeReceiptView {
        for _ in 0..polls {
            let view = native_receipt_inner(state, request_id)
                .await
                .expect("receipt read");
            if done(&view) {
                return view;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        panic!("request {request_id} did not reach the expected state in {polls} s");
    }

    fn status(view: &NativeReceiptView) -> Option<&str> {
        view.receipt.as_ref()?.get("observed_status")?.as_str()
    }

    #[tokio::test]
    #[ignore = "needs a running protocol-4 chain that funds ARC_LIVE_PHRASE"]
    async fn a_paid_request_settles_or_refunds_on_a_real_chain() {
        let phrase = std::env::var("ARC_LIVE_PHRASE").expect("ARC_LIVE_PHRASE");
        std::env::var("ARC_WALLET_HOST").expect("ARC_WALLET_HOST names the chain host");
        let refund = std::env::var("ARC_LIVE_EXPECT").is_ok_and(|v| v == "refund");
        let identity = crate::identity::derive(&phrase).expect("identity from phrase");
        let address = strip_0x(&identity.address);
        let dir = tempfile::tempdir().unwrap();
        let app = state(dir.path(), identity.clone());

        let context = native_context_inner(&app)
            .await
            .unwrap()
            .expect("a protocol-4 host");
        assert!(context.compatible, "{:?}", context.reason);
        let prepared = native_prepare_inner(&app, "live journey").await.unwrap();
        let host = wallet::validate_rpc_origin(&crate::commands::chain_host(&app).await).unwrap();
        let before = rpc_client::fetch_balance(&app.http, &host, &address)
            .await
            .unwrap();

        let submitted = native_submit_inner(
            &app,
            NativeSubmitRequest {
                input_hex: prepared.input_hex.clone(),
                input_kind: prepared.input_kind.clone(),
                prompt_preview: "live journey".into(),
                max_tokens: 8,
                price_arc: "0.01".into(),
                reserve_arc: "0.02".into(),
                expiry_blocks: if refund { 120 } else { 2_000 },
            },
        )
        .await
        .unwrap();
        assert!(submitted.accepted, "{submitted:?}");
        // A second request before the first is in a block would compete for
        // the nonce: the core refuses it before signing.
        let queued = native_submit_inner(
            &app,
            NativeSubmitRequest {
                input_hex: prepared.input_hex.clone(),
                input_kind: prepared.input_kind.clone(),
                prompt_preview: "second".into(),
                max_tokens: 8,
                price_arc: "0.01".into(),
                reserve_arc: "0.02".into(),
                expiry_blocks: 600,
            },
        )
        .await;
        if let Err(message) = &queued {
            assert!(message.contains("not in a block yet"), "{message}");
        }

        let id = submitted.request_id.clone();
        // Printed so the documented cross-surface check (runbook section 6.6)
        // can be pointed at this request: the journal this test writes lives in
        // a temporary directory that is deleted when the test ends, and the
        // node exposes no endpoint that enumerates request ids, so without this
        // line a refunded request cannot be handed to `arc_ops.receipts` or
        // `explorer/test-live-native.mjs`. Visible with `-- --nocapture`.
        println!("live_journey request_id={id}");
        let price = 10_000_000u64;
        let reserve = 20_000_000u64;
        let settled = if refund {
            let due = wait(&app, &id, 600, |view| {
                status(view) == Some("Pending")
                    && view
                        .receipt
                        .as_ref()
                        .and_then(|r| r.get("expires_at"))
                        .and_then(Value::as_u64)
                        .is_some_and(|expires| view.height + 1 >= expires)
            })
            .await;
            assert_eq!(status(&due), Some("Pending"));
            let claim = native_refund_inner(&app, &id).await.unwrap();
            assert!(claim.accepted, "{claim:?}");
            wait(&app, &id, 300, |view| status(view) == Some("Refunded")).await
        } else {
            wait(&app, &id, 300, |view| status(view) == Some("Finalized")).await
        };
        let receipt = settled.receipt.unwrap();
        let credits: u64 = receipt["settlement_credits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["amount"].as_u64().unwrap())
            .sum();
        assert_eq!(credits, reserve, "credits add up to the reservation");

        // The wallet pays exactly the price on a finalized request, nothing on
        // a refunded one (native transactions carry no fee). If the second
        // request was admitted meanwhile, its reservation is also held.
        let after = rpc_client::fetch_balance(&app.http, &host, &address)
            .await
            .unwrap();
        let before: u64 = before.balance_base.parse().unwrap();
        let after: u64 = after.balance_base.parse().unwrap();
        let expected = if refund { before } else { before - price };
        let mut acceptable = vec![expected];
        if queued.is_ok() {
            // The second request was signed once the first was in a block:
            // its reservation may be held, or (settle path) it finalized too.
            acceptable.push(expected - reserve);
            if !refund {
                acceptable.push(expected - price);
            }
        }
        assert!(
            acceptable.contains(&after),
            "balance {after}, expected one of {acceptable:?}"
        );

        // A restart finds the signed request in the journal, no longer
        // resubmittable once its nonce is used.
        let restarted = state(dir.path(), identity);
        let journal = native_journal_inner(&restarted).await.unwrap();
        let entry = journal
            .iter()
            .find(|e| e.request_id == id)
            .expect("journaled");
        assert!(!entry.resubmittable);
        if refund {
            assert!(journal
                .iter()
                .any(|e| e.kind == RecordKind::Refund && e.request_id == id));
        }

        // A protocol-4 block carries only native transactions, so the wallet
        // refuses a transfer before signing anything, and the journal (which
        // holds the account's nonce for native transactions) is untouched.
        let before = Journal::load(dir.path()).unwrap().records.len();
        let recipient = hash_bytes(b"live-journey-transfer-recipient").to_hex();
        let refused = crate::commands::send_arc_inner(&restarted, recipient, "0.001".into())
            .await
            .expect_err("a transfer on a protocol-4 chain");
        assert!(refused.contains("nothing was signed"), "{refused}");
        assert_eq!(Journal::load(dir.path()).unwrap().records.len(), before);
    }
}
