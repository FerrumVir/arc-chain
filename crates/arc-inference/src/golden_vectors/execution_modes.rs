//! Execution-mode equivalence on the golden fixture model.
//!
//! Optimistic release, teacher-forced validator re-checks, cross-node KV and
//! prefix-cache reuse, and speculative decoding all rest on one property: the
//! engine computes the same bits however a sequence is scheduled. Each test
//! here holds one family of modes to the token-by-token run, value for value:
//!
//! 1. token by token (`forward_one_token`, the worker's decode loop);
//! 2. one batched pass over the whole sequence (a prefill, or the
//!    teacher-forced re-check of a finished generation);
//! 3. chunked prefill, at chunk sizes from 1 to the whole sequence;
//! 4. resuming from a KV prefix restored from bytes on another model instance;
//! 5. speculative verification of k drafted tokens in one pass, accepted and
//!    rejected, with the rejected rows rolled back;
//! 6. stage splits: every 1-, 2-, 3- and 4-way split of the four layers, with
//!    one KV cache per stage holder, one row per call (`forward_shard_token`)
//!    or k rows per call (`forward_shard_rows`), with rejected rows rolled
//!    back on every holder (`rollback_rows`);
//! 7. rayon pools of 1, 2 and N threads, with the scalar projection kernel
//!    and with the vectorised kernels: every multi-row kernel the runner's
//!    CPU has (`neon-sdot-limb` and `neon-i8mm-limb` on arm64; `avx2-limb`,
//!    `avx-vnni-limb` and `avx512-vnni-limb` on x86-64), each its own leg.
//!
//! Every mode is compared on each position's raw logits, the residual stream
//! leaving every layer (the per-layer boundary hash), every K and V row, and
//! the generated tokens with their output hash. Both canonical INT8 profiles
//! run: the live legacy split-half profile (`CANONICAL_REWARD_INFERENCE_PROFILE`)
//! and the GGUF interleaved-RoPE profile. The token-by-token reference is
//! itself pinned to the reviewed constants in `integer_inference_kat.json` and
//! `integer_operator_kat.json`, so agreement is never only between two runs of
//! the same code.
//!
//! A failure names the mode, the leg (profile, kernel, threads) and the first
//! differing position and layer.

use super::{
    GoldenFixture, build_fixture_model, fixture, hash_cache, hash_i64, operators, text, token_ids,
};
use crate::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel,
    GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, I8Weights, KVCache, ShardInput, ShardOutput,
    ShardRowsError, ShardRowsInput, ShardRowsOutput, select_next_token_with_repetition_penalty,
};
use crate::canonical_simd::{self, BatchedKernel};
use arc_crypto::hash_bytes;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::MutexGuard;

// ── Profiles, kernels and thread counts ─────────────────────────────────────

/// The two canonical per-row INT8 arithmetic profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    /// `CANONICAL_REWARD_INFERENCE_PROFILE`: the live network profile, which
    /// workers load and shard holders are pinned to.
    LegacySplitHalf,
    /// `GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE`.
    GgufInterleaved,
}

const PROFILES: [Profile; 2] = [Profile::LegacySplitHalf, Profile::GgufInterleaved];

/// Builds the KAT recipe in `profile` and checks its execution identity.
fn build_model(fixture: &GoldenFixture, profile: Profile) -> CachedIntegerModel {
    let mut model = build_fixture_model(fixture);
    let identity = match profile {
        Profile::LegacySplitHalf => CANONICAL_REWARD_INFERENCE_PROFILE,
        Profile::GgufInterleaved => {
            model
                .canonicalize_gguf_interleaved_rope_rows()
                .expect("the fixture is a complete canonical I8 model");
            GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
        }
    };
    assert_eq!(model.canonical_execution_profile(), Some(identity));
    model
}

/// The projection kernel a leg forces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kernel {
    Scalar,
    /// The exact limb kernels with this multi-row kernel selected. One-row
    /// projections run it too when it is a matrix-extension kernel (SMMLA,
    /// VNNI); with SDOT or AVX2 they run the one-row kernel of that ISA.
    Vectorised(BatchedKernel),
}

/// One execution environment: a projection kernel and a rayon pool size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Leg {
    kernel: Kernel,
    threads: usize,
}

/// The leg every reference run uses.
const BASE_LEG: Leg = Leg {
    kernel: Kernel::Scalar,
    threads: 1,
};

/// 1, 2 and N threads, N being every core of the runner (at least 4).
fn thread_counts() -> [usize; 3] {
    let cores = std::thread::available_parallelism().map_or(4, |cores| cores.get());
    [1, 2, cores.max(4)]
}

/// Exclusive use of the process-wide kernel switches for one test, restored
/// on drop. Holding the lock also keeps this suite's batched prefills out of
/// the prefill census that tests in `cached_integer_model` assert on.
struct KernelSwitch {
    _lock: MutexGuard<'static, ()>,
    previous: bool,
    previous_batched: Option<BatchedKernel>,
    pools: BTreeMap<usize, ThreadPool>,
}

impl KernelSwitch {
    fn hold() -> Self {
        let lock = canonical_simd::kernel_switch_guard();
        // The first read applies any environment default, so the explicit
        // choices `run` makes afterwards are the ones that hold.
        let previous = canonical_simd::fast_canonical_kernel_enabled();
        let previous_batched = canonical_simd::batched_kernel_preference();
        let pools = thread_counts()
            .into_iter()
            .map(|threads| {
                let pool = ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .expect("test pool");
                (threads, pool)
            })
            .collect();
        let switch = Self {
            _lock: lock,
            previous,
            previous_batched,
            pools,
        };
        // Printed under --nocapture so CI logs show which legs really ran.
        println!("golden_modes legs on this runner: {:?}", switch.legs());
        switch
    }

    /// Every leg this machine can run. The vectorised kernels run only where
    /// the CPU has them, one leg per multi-row kernel; elsewhere the scalar
    /// kernel is the only kernel in use.
    fn legs(&self) -> Vec<Leg> {
        let mut kernels = vec![Kernel::Scalar];
        if canonical_simd::dotprod_available() {
            kernels.extend(
                canonical_simd::available_batched_kernels()
                    .into_iter()
                    .map(Kernel::Vectorised),
            );
        }
        kernels
            .into_iter()
            .flat_map(|kernel| {
                self.pools
                    .keys()
                    .map(move |&threads| Leg { kernel, threads })
            })
            .collect()
    }

    /// Runs `work` on the leg's pool with the leg's kernels selected.
    fn run<R: Send>(&self, leg: Leg, work: impl FnOnce() -> R + Send) -> R {
        let batched = match leg.kernel {
            Kernel::Scalar => None,
            Kernel::Vectorised(kernel) => Some(kernel),
        };
        canonical_simd::set_fast_canonical_kernel(batched.is_some());
        canonical_simd::set_batched_kernel_preference(batched);
        assert_eq!(
            canonical_simd::fast_canonical_kernel_enabled(),
            batched.is_some(),
            "the kernel switch did not take effect for {leg:?}"
        );
        if batched.is_some() {
            assert_eq!(
                canonical_simd::selected_batched_kernel(),
                batched,
                "the multi-row kernel switch did not take effect for {leg:?}"
            );
        }
        self.pools[&leg.threads].install(work)
    }
}

impl Drop for KernelSwitch {
    fn drop(&mut self) {
        // Printed under --nocapture: how many multi-row projections each
        // kernel has run in this process so far, so CI logs show the
        // kernels really ran.
        println!(
            "golden_modes multi-row projections by kernel so far: {}",
            canonical_simd::batched_kernel_run_report()
        );
        canonical_simd::set_fast_canonical_kernel(self.previous);
        canonical_simd::set_batched_kernel_preference(self.previous_batched);
    }
}

// ── What a mode reports ─────────────────────────────────────────────────────

/// What one execution mode reported, keyed by absolute position.
#[derive(Default)]
struct Trace {
    /// Raw logits of every position the mode returned logits for.
    logits: BTreeMap<usize, Vec<i64>>,
    /// BLAKE3 of the residual stream leaving layer `l` at position `p`, keyed
    /// `(p, l)`: the per-layer boundary hash.
    boundaries: BTreeMap<(usize, usize), String>,
}

impl Trace {
    fn observe(&mut self, position: usize, layer: usize, hidden: &[i64]) {
        let previous = self.boundaries.insert((position, layer), hash_i64(hidden));
        assert!(
            previous.is_none(),
            "layer {layer} at position {position} was reported twice"
        );
    }

    fn record_logits(&mut self, position: usize, logits: Vec<i64>) {
        assert!(!logits.is_empty(), "position {position} returned no logits");
        let previous = self.logits.insert(position, logits);
        assert!(
            previous.is_none(),
            "position {position} returned logits twice"
        );
    }

    /// Moves the rows of `positions` out of `round` and drops the rest.
    fn keep(&mut self, round: Trace, positions: &Range<usize>) {
        for (position, logits) in round.logits {
            if positions.contains(&position) {
                self.record_logits(position, logits);
            }
        }
        for ((position, layer), hash) in round.boundaries {
            if positions.contains(&position) {
                let previous = self.boundaries.insert((position, layer), hash);
                assert!(
                    previous.is_none(),
                    "layer {layer} at position {position} was kept twice"
                );
            }
        }
    }
}

// ── Modes 1, 2 and 3: token by token, one pass, chunked ─────────────────────

/// How a run schedules the positions it feeds.
#[derive(Clone, Copy, Debug)]
enum Schedule {
    /// Mode 1: one `forward_one_token` call per position.
    TokenByToken,
    /// Modes 2 and 3: the canonical batched prefill in chunks of this many
    /// positions, returning every position's logits (the verification shape).
    Chunked(usize),
    /// The batched prefill returning only the last position's logits, which
    /// is how serving prefill calls it.
    LastOnly(usize),
}

/// Feeds `tokens` on top of `kv` with `schedule`, recording into `trace`.
fn feed(
    model: &CachedIntegerModel,
    schedule: Schedule,
    tokens: &[u32],
    kv: &mut KVCache,
    trace: &mut Trace,
) {
    match schedule {
        Schedule::TokenByToken => {
            for &token in tokens {
                let position = kv.seq_len;
                let logits =
                    model.forward_one_token_observed(token, kv, |p, l, h| trace.observe(p, l, h));
                trace.record_logits(position, logits);
            }
        }
        Schedule::Chunked(chunk) => prefill(model, tokens, kv, chunk, true, trace),
        Schedule::LastOnly(chunk) => prefill(model, tokens, kv, chunk, false, trace),
    }
}

fn prefill(
    model: &CachedIntegerModel,
    tokens: &[u32],
    kv: &mut KVCache,
    chunk: usize,
    all_positions: bool,
    trace: &mut Trace,
) {
    let first = kv.seq_len;
    let rows = model
        .prefill_canonical_i8_batched_observed(tokens, kv, chunk, all_positions, |p, l, h| {
            trace.observe(p, l, h)
        })
        .unwrap_or_else(|| {
            panic!(
                "the canonical batched prefill refused {} tokens at position {first} in chunks of {chunk}",
                tokens.len()
            )
        });
    let (first_row, row_count) = if all_positions {
        (first, tokens.len())
    } else {
        (first + tokens.len() - 1, 1)
    };
    assert_eq!(rows.len(), row_count, "batched prefill row count");
    for (offset, logits) in rows.into_iter().enumerate() {
        trace.record_logits(first_row + offset, logits);
    }
}

/// [`feed`] from position 0 on an empty cache.
fn run_fresh(model: &CachedIntegerModel, schedule: Schedule, tokens: &[u32]) -> (Trace, KVCache) {
    let mut kv = empty_kv(model);
    let mut trace = Trace::default();
    feed(model, schedule, tokens, &mut kv, &mut trace);
    (trace, kv)
}

// ── KV state: copies, rollback and a byte image ─────────────────────────────

fn empty_kv(model: &CachedIntegerModel) -> KVCache {
    KVCache::new(model.config.n_layers)
}

fn clone_kv(kv: &KVCache) -> KVCache {
    KVCache {
        k_data: kv.k_data.clone(),
        v_data: kv.v_data.clone(),
        seq_len: kv.seq_len,
    }
}

/// Drops every row at or after `positions`, as rolling back rejected
/// speculative rows requires.
fn truncate_kv(kv: &mut KVCache, positions: usize, d_kv: usize) {
    assert!(
        positions <= kv.seq_len,
        "cannot roll a {}-position cache forward to {positions}",
        kv.seq_len
    );
    for rows in kv.k_data.iter_mut().chain(kv.v_data.iter_mut()) {
        rows.truncate(positions * d_kv);
    }
    kv.seq_len = positions;
}

/// Magic and version of this suite's KV image. The engine has no KV codec;
/// this one writes exactly the state `KVCache` holds. A restored cache that
/// resumes bit-identically shows that the logits depend on no other request
/// state. Selecting tokens also needs the generated history (the repetition
/// penalty), which a resuming node must be given separately.
const KV_IMAGE_MAGIC: [u8; 8] = *b"ARCKVv01";

/// Serializes `kv`: the magic, the layer count and `seq_len`, then each
/// layer's K rows and V rows as a counted list of little-endian `i64` values.
fn kv_to_bytes(kv: &KVCache) -> Vec<u8> {
    let mut bytes = KV_IMAGE_MAGIC.to_vec();
    bytes.extend_from_slice(&(kv.k_data.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(kv.seq_len as u64).to_le_bytes());
    for (keys, values) in kv.k_data.iter().zip(&kv.v_data) {
        for rows in [keys, values] {
            bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
            for value in rows {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    bytes
}

/// Reads the eight-byte words of a KV image, refusing a truncated one.
struct ImageReader<'a>(&'a [u8]);

impl ImageReader<'_> {
    fn word(&mut self) -> [u8; 8] {
        let (head, rest) = self.0.split_first_chunk::<8>().expect("truncated KV image");
        self.0 = rest;
        *head
    }

    fn count(&mut self) -> usize {
        usize::try_from(u64::from_le_bytes(self.word())).expect("KV image count overflows usize")
    }

    fn rows(&mut self) -> Vec<i64> {
        let count = self.count();
        assert!(
            count <= self.0.len() / 8,
            "KV image row count exceeds the image"
        );
        (0..count)
            .map(|_| i64::from_le_bytes(self.word()))
            .collect()
    }
}

/// Parses an image written by [`kv_to_bytes`], as a node restoring a shipped
/// prefix would.
fn kv_from_bytes(bytes: &[u8]) -> KVCache {
    let mut reader = ImageReader(bytes);
    assert_eq!(reader.word(), KV_IMAGE_MAGIC, "not a KV image");
    let n_layers = reader.count();
    let seq_len = reader.count();
    let mut k_data = Vec::new();
    let mut v_data = Vec::new();
    for _ in 0..n_layers {
        k_data.push(reader.rows());
        v_data.push(reader.rows());
    }
    assert!(reader.0.is_empty(), "trailing bytes after the KV image");
    KVCache {
        k_data,
        v_data,
        seq_len,
    }
}

// ── The reference and the comparisons ──────────────────────────────────────

/// The token-by-token run, on the base leg, that every other mode must equal.
struct Reference {
    trace: Trace,
    kv: KVCache,
    d_kv: usize,
}

impl Reference {
    fn new(model: &CachedIntegerModel, tokens: &[u32]) -> Self {
        let (trace, kv) = run_fresh(model, Schedule::TokenByToken, tokens);
        Self {
            trace,
            kv,
            d_kv: model.config.d_kv,
        }
    }

    fn n_layers(&self) -> usize {
        self.kv.k_data.len()
    }

    /// Checks what a mode reported for `positions`: every layer boundary of a
    /// position, then its logits when it returned logits (every position from
    /// `logits_from` on). Panics at the first difference in (position, layer)
    /// order.
    fn assert_rows(&self, got: &Trace, positions: Range<usize>, logits_from: usize, mode: &str) {
        let n_layers = self.n_layers();
        assert_eq!(
            got.boundaries.len(),
            positions.len() * n_layers,
            "{mode}: wrong number of layer boundaries"
        );
        assert_eq!(
            got.logits.len(),
            positions.end - logits_from,
            "{mode}: wrong number of logit rows"
        );
        for position in positions {
            for layer in 0..n_layers {
                let actual = got.boundaries.get(&(position, layer)).unwrap_or_else(|| {
                    panic!("{mode}: no boundary at position {position}, layer {layer}")
                });
                let expected = &self.trace.boundaries[&(position, layer)];
                assert!(
                    actual == expected,
                    "{mode} DIFFERS from token by token: first difference at position \
                     {position}, layer {layer} (boundary hash {actual}, expected {expected})"
                );
            }
            if position >= logits_from {
                let actual = got
                    .logits
                    .get(&position)
                    .unwrap_or_else(|| panic!("{mode}: no logits at position {position}"));
                let expected = &self.trace.logits[&position];
                assert!(
                    actual == expected,
                    "{mode} DIFFERS from token by token: logits at position {position} \
                     (digest {}, expected {}) although every layer boundary matched",
                    hash_i64(actual),
                    hash_i64(expected)
                );
            }
        }
    }

    /// Checks every K and V row of `got`, which must hold exactly `positions`
    /// positions. Panics at the first differing position, then layer.
    fn assert_kv(&self, got: &KVCache, positions: usize, mode: &str) {
        let (n_layers, d_kv) = (self.n_layers(), self.d_kv);
        assert_eq!(got.seq_len, positions, "{mode}: KV seq_len");
        assert_eq!(got.k_data.len(), n_layers, "{mode}: K layer count");
        assert_eq!(got.v_data.len(), n_layers, "{mode}: V layer count");
        for position in 0..positions {
            let row = position * d_kv..(position + 1) * d_kv;
            for layer in 0..n_layers {
                let tensors = [
                    ("K", &got.k_data[layer], &self.kv.k_data[layer]),
                    ("V", &got.v_data[layer], &self.kv.v_data[layer]),
                ];
                for (name, actual, expected) in tensors {
                    assert!(
                        actual.get(row.clone()) == expected.get(row.clone()),
                        "{mode} DIFFERS from token by token: first KV difference at position \
                         {position}, layer {layer} ({name} row)"
                    );
                }
            }
        }
        for (layer, (keys, values)) in got.k_data.iter().zip(&got.v_data).enumerate() {
            assert_eq!(
                (keys.len(), values.len()),
                (positions * d_kv, positions * d_kv),
                "{mode}: layer {layer} holds K or V rows past position {positions}"
            );
        }
    }

    /// Checks a stage pipeline over `positions`: the hidden state handed
    /// across every stage boundary, then the terminal stage's token and logits
    /// hash, which select with `history(position)` as the shard RPC does.
    fn assert_stages(
        &self,
        got: &StageTrace,
        ends: &[usize],
        positions: Range<usize>,
        history: impl Fn(usize) -> Vec<u32>,
        mode: &str,
    ) {
        let cuts = &ends[..ends.len() - 1];
        assert_eq!(
            got.boundaries.len(),
            positions.len() * cuts.len(),
            "{mode}: wrong number of stage boundaries"
        );
        assert_eq!(
            got.terminal.len(),
            positions.len(),
            "{mode}: wrong number of terminal outputs"
        );
        for position in positions {
            for &end in cuts {
                let layer = end - 1;
                let actual = got.boundaries.get(&(position, layer)).unwrap_or_else(|| {
                    panic!("{mode}: no stage boundary at position {position}, layer {layer}")
                });
                let expected = &self.trace.boundaries[&(position, layer)];
                assert!(
                    actual == expected,
                    "{mode} DIFFERS from token by token: first difference at position \
                     {position}, layer {layer} (stage boundary hash {actual}, expected {expected})"
                );
            }
            let (token, logits_hash) = &got.terminal[&position];
            let logits = &self.trace.logits[&position];
            let selected = history(position);
            assert!(
                *logits_hash == penalized_hash(logits, &selected),
                "{mode} DIFFERS from token by token: terminal logits hash at position {position}"
            );
            assert_eq!(
                *token,
                select(logits, &selected),
                "{mode}: terminal token at position {position}"
            );
        }
    }
}

// ── Token selection and generation ──────────────────────────────────────────

/// ARC's token selection: greedy argmax after the deterministic repetition
/// penalty, shared by whole-model generation and terminal stages.
fn select(logits: &[i64], history: &[u32]) -> u32 {
    let mut logits = logits.to_vec();
    select_next_token_with_repetition_penalty(&mut logits, history)
}

/// The logits hash a terminal stage commits: taken after the penalty.
fn penalized_hash(logits: &[i64], history: &[u32]) -> String {
    let mut logits = logits.to_vec();
    select_next_token_with_repetition_penalty(&mut logits, history);
    hash_i64(&logits)
}

fn output_hash(tokens: &[u32]) -> String {
    let bytes: Vec<u8> = tokens
        .iter()
        .flat_map(|token| token.to_le_bytes())
        .collect();
    hex::encode(hash_bytes(&bytes).0)
}

/// The two whole-model generation semantics in production.
#[derive(Clone, Copy, Debug)]
enum Generation {
    /// `try_generate`, the community worker's call: BOS, the prompt, the last
    /// prompt token once more, then every generated token but the last.
    Worker,
    /// `try_generate_v2`: the prompt's own final logits select the first
    /// token, and every generated token is fed back.
    V2,
}

const GENERATIONS: [Generation; 2] = [Generation::Worker, Generation::V2];

/// The production generation call, on whatever leg runs it.
fn production_generate(
    model: &CachedIntegerModel,
    generation: Generation,
    prompt: &[u32],
    max_tokens: u32,
) -> (Vec<u32>, String) {
    let (tokens, hash) = match generation {
        Generation::Worker => model.try_generate(prompt, max_tokens, &[]),
        Generation::V2 => model.try_generate_v2(prompt, max_tokens, &[]),
    }
    .expect("the generation fits the fixture's context window");
    (tokens, hex::encode(hash.0))
}

/// The prompt and budget of each profile's reviewed generation vector.
fn generation_case(fixture: &GoldenFixture, profile: Profile) -> (Vec<u32>, u32) {
    match profile {
        Profile::LegacySplitHalf => (
            fixture.generation_prompt.clone(),
            fixture.generation_max_tokens,
        ),
        Profile::GgufInterleaved => {
            let doc = operators();
            let run = &doc["interleaved_generation_v2"]["generation_v2"][0];
            let max_tokens = run["max_tokens"].as_u64().expect("max_tokens");
            (
                token_ids(&run["prompt"]),
                u32::try_from(max_tokens).expect("max_tokens fits u32"),
            )
        }
    }
}

/// Pins a production generation to the reviewed vectors where they exist.
fn pin_generation(
    fixture: &GoldenFixture,
    profile: Profile,
    generation: Generation,
    tokens: &[u32],
    hash: &str,
) {
    match (profile, generation) {
        (Profile::LegacySplitHalf, Generation::Worker) => {
            assert_eq!(tokens, fixture.expected.generated_tokens.as_slice());
            assert_eq!(hash, fixture.expected.generated_output_hash);
        }
        (Profile::GgufInterleaved, Generation::V2) => {
            let doc = operators();
            let run = &doc["interleaved_generation_v2"]["generation_v2"][0];
            assert_eq!(tokens, token_ids(&run["tokens"]).as_slice());
            assert_eq!(hash, text(&run["output_hash"]));
        }
        (Profile::LegacySplitHalf, Generation::V2)
        | (Profile::GgufInterleaved, Generation::Worker) => {
            let pins = pinned();
            let run = &pins["generation"][format!("{profile:?}/{generation:?}").as_str()];
            let (prompt, max_tokens) = generation_case(fixture, profile);
            assert_eq!(token_ids(&run["prompt"]), prompt, "pinned prompt");
            assert_eq!(run["max_tokens"], max_tokens, "pinned budget");
            assert_eq!(
                tokens,
                token_ids(&run["tokens"]).as_slice(),
                "{profile:?} {generation:?}: tokens drifted from the pinned reference"
            );
            assert_eq!(
                hash,
                text(&run["output_hash"]),
                "{profile:?} {generation:?}: output hash drifted from the pinned reference"
            );
        }
    }
}

/// Every token a generation fed, in order, and the position whose logits
/// selected the first generated token. A teacher-forced re-check feeds
/// exactly this sequence in one pass.
fn fed_sequence(
    model: &CachedIntegerModel,
    generation: Generation,
    prompt: &[u32],
    generated: &[u32],
) -> (Vec<u32>, usize) {
    let last_prompt = *prompt.last().expect("a non-empty prompt");
    let (_, all_but_last) = generated.split_last().expect("a non-empty generation");
    let mut fed = vec![model.config.bos_token];
    fed.extend_from_slice(prompt);
    match generation {
        Generation::Worker => {
            fed.push(last_prompt);
            fed.extend_from_slice(all_but_last);
            (fed, prompt.len() + 1)
        }
        Generation::V2 => {
            fed.extend_from_slice(generated);
            (fed, prompt.len())
        }
    }
}

/// Re-derives `count` generated tokens from per-position logits, as a
/// validator re-checks a worker: the same selection rule and history.
fn recheck(trace: &Trace, first_row: usize, count: usize) -> Vec<u32> {
    let mut tokens = Vec::with_capacity(count);
    for row in first_row..first_row + count {
        let next = select(&trace.logits[&row], &tokens);
        tokens.push(next);
    }
    tokens
}

// ── Mode 5: speculative verification ────────────────────────────────────────

/// Where a simulated draft model proposes a wrong token. Errors are placed by
/// round and by draft slot within the round, never by generated index, so
/// every pattern but `AllRight` is rejected in round 0 whatever k is.
#[derive(Clone, Copy, Debug)]
enum Drafts {
    /// Every draft is the model's own next token.
    AllRight,
    /// Every round's first draft is wrong, so each round is rejected whole.
    AllWrong,
    /// Every round's last draft is wrong: the drafts before it are accepted
    /// and one row is rolled back.
    LastWrong,
    /// Even rounds go wrong half-way through their drafts; odd rounds are
    /// right.
    MiddleWrongEvenRounds,
}

impl Drafts {
    fn wrong(self, round: usize, slot: usize, drafted: usize) -> bool {
        match self {
            Drafts::AllRight => false,
            Drafts::AllWrong => true,
            Drafts::LastWrong => slot + 1 == drafted,
            Drafts::MiddleWrongEvenRounds => round.is_multiple_of(2) && slot == drafted / 2,
        }
    }
}

const DRAFT_PATTERNS: [Drafts; 4] = [
    Drafts::AllRight,
    Drafts::AllWrong,
    Drafts::LastWrong,
    Drafts::MiddleWrongEvenRounds,
];

struct Speculation {
    tokens: Vec<u32>,
    trace: Trace,
    kv: KVCache,
    counts: SpeculationCounts,
}

/// What a speculative decode did with its drafts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SpeculationCounts {
    accepted_drafts: usize,
    rejected_rounds: usize,
    rolled_back_rows: usize,
}

/// The counts a speculative decode must reach when the model's selection is
/// always `truth`, worked out from the draft pattern alone, without the
/// model. A run that accepts or rolls back anything else verified the wrong
/// rows.
fn planned_speculation(
    generation: Generation,
    max_tokens: usize,
    drafts: Drafts,
    k: usize,
) -> SpeculationCounts {
    let mut emitted = match generation {
        Generation::Worker => 0,
        Generation::V2 => 1,
    };
    let mut counts = SpeculationCounts::default();
    let mut round = 0;
    while emitted < max_tokens {
        let drafted = k.min(max_tokens - emitted - 1);
        match (0..drafted).find(|&slot| drafts.wrong(round, slot, drafted)) {
            Some(slot) => {
                counts.accepted_drafts += slot;
                counts.rejected_rounds += 1;
                counts.rolled_back_rows += drafted - slot;
                emitted += slot + 1;
            }
            None => {
                counts.accepted_drafts += drafted;
                emitted += drafted + 1;
            }
        }
        round += 1;
    }
    counts
}

/// A speculative decode. Each round, a simulated draft model proposes up to
/// `k` tokens (copies of `truth`, wrong where `drafts` says so). The model
/// feeds the pending token and every draft in one batched pass, keeps the
/// rows up to the last accepted draft, rolls the rejected rows out of the KV
/// cache, and emits its own selection after the last accepted draft.
fn speculate(
    model: &CachedIntegerModel,
    generation: Generation,
    prompt: &[u32],
    truth: &[u32],
    drafts: Drafts,
    k: usize,
) -> Speculation {
    let config = &model.config;
    let vocab = u32::try_from(config.vocab_size).expect("vocabulary fits u32");
    let max_tokens = truth.len();
    let mut kv = empty_kv(model);
    let mut trace = Trace::default();
    let mut prompt_rows = vec![config.bos_token];
    prompt_rows.extend_from_slice(prompt);
    prefill(
        model,
        &prompt_rows,
        &mut kv,
        prompt_rows.len(),
        true,
        &mut trace,
    );

    let mut tokens: Vec<u32> = Vec::with_capacity(max_tokens);
    let mut pending = match generation {
        Generation::Worker => *prompt.last().expect("a non-empty prompt"),
        Generation::V2 => {
            let first = select(&trace.logits[&prompt.len()], &tokens);
            tokens.push(first);
            first
        }
    };
    let mut counts = SpeculationCounts::default();
    let mut round = 0;
    while tokens.len() < max_tokens {
        // Accepted drafts plus the model's own next token never exceed the budget.
        let n_drafts = k.min(max_tokens - tokens.len() - 1);
        let first = tokens.len();
        let mut batch = vec![pending];
        batch.extend((0..n_drafts).map(|slot| {
            let right = truth[first + slot];
            if drafts.wrong(round, slot, n_drafts) {
                (right + 1) % vocab
            } else {
                right
            }
        }));
        let base = kv.seq_len;
        let mut verified = Trace::default();
        prefill(model, &batch, &mut kv, batch.len(), true, &mut verified);

        // Row `j` holds the logits after `batch[j]`; it selects what follows.
        let mut kept = 1;
        let mut next = select(&verified.logits[&base], &tokens);
        while kept < batch.len() && batch[kept] == next {
            tokens.push(next);
            next = select(&verified.logits[&(base + kept)], &tokens);
            kept += 1;
        }
        counts.accepted_drafts += kept - 1;
        if kept < batch.len() {
            counts.rejected_rounds += 1;
            counts.rolled_back_rows += batch.len() - kept;
            truncate_kv(&mut kv, base + kept, config.d_kv);
        }
        trace.keep(verified, &(base..base + kept));
        tokens.push(next);
        pending = next;
        round += 1;
    }
    if matches!(generation, Generation::V2) {
        // generate_v2 feeds every generated token back, the last one included.
        prefill(model, &[pending], &mut kv, 1, true, &mut trace);
    }
    Speculation {
        tokens,
        trace,
        kv,
        counts,
    }
}

// ── Mode 6: stage splits ─────────────────────────────────────────────────────

/// What a stage pipeline reported, keyed by absolute position.
#[derive(Default)]
struct StageTrace {
    /// Hash of the hidden state a stage ending after layer `l` handed on at
    /// position `p`, keyed `(p, l)`.
    boundaries: BTreeMap<(usize, usize), String>,
    /// The terminal stage's token and logits hash at each position.
    terminal: BTreeMap<usize, (u32, String)>,
}

/// Every way to cut `n_layers` layers into contiguous stages, each written as
/// the exclusive end layer of every stage.
fn stage_splits(n_layers: usize) -> Vec<Vec<usize>> {
    (0..(1usize << (n_layers - 1)))
        .map(|cuts| {
            (1..n_layers)
                .filter(|&layer| cuts & (1 << (layer - 1)) != 0)
                .chain([n_layers])
                .collect()
        })
        .collect()
}

/// Feeds one position through stage holders that each keep their own KV
/// cache, as separate nodes do, and returns the terminal stage's token.
/// `history` reaches only the terminal stage, as the shard RPC requires.
fn stage_step(
    model: &CachedIntegerModel,
    ends: &[usize],
    holders: &mut [KVCache],
    position: usize,
    token: u32,
    history: &[u32],
    trace: &mut StageTrace,
) -> u32 {
    assert_eq!(ends.len(), holders.len());
    let mut carried: Option<Vec<i64>> = None;
    let mut start = 0;
    for (&end, holder) in ends.iter().zip(holders.iter_mut()) {
        let input = match carried.take() {
            None => ShardInput::Token(token),
            Some(hidden) => ShardInput::Hidden(hidden),
        };
        let terminal = end == model.config.n_layers;
        let stage_history: &[u32] = if terminal { history } else { &[] };
        let output = model
            .forward_shard_token_with_history(input, holder, start, end, position, stage_history)
            .unwrap_or_else(|error| {
                panic!("stage [{start}, {end}) refused position {position}: {error}")
            });
        match output {
            ShardOutput::Hidden(hidden) => {
                let previous = trace
                    .boundaries
                    .insert((position, end - 1), hash_i64(&hidden));
                assert!(
                    previous.is_none(),
                    "stage boundary {end} at position {position} reported twice"
                );
                carried = Some(hidden);
            }
            ShardOutput::Token { id, logits_hash } => {
                assert!(terminal, "stage [{start}, {end}) is not terminal");
                let previous = trace
                    .terminal
                    .insert(position, (id, hex::encode(logits_hash.0)));
                assert!(
                    previous.is_none(),
                    "position {position} returned a token twice"
                );
                return id;
            }
        }
        start = end;
    }
    panic!("the last stage of {ends:?} returned a hidden state, not a token");
}

fn stage_holders(model: &CachedIntegerModel, ends: &[usize]) -> Vec<KVCache> {
    ends.iter().map(|_| empty_kv(model)).collect()
}

/// Joins the stage holders' caches layer by layer, checking that no holder
/// wrote a layer it does not hold.
fn merge_holders(ends: &[usize], holders: &[KVCache]) -> KVCache {
    let n_layers = holders[0].k_data.len();
    let seq_len = holders[0].seq_len;
    let mut merged = KVCache::new(n_layers);
    merged.seq_len = seq_len;
    let mut start = 0;
    for (&end, holder) in ends.iter().zip(holders) {
        assert_eq!(holder.seq_len, seq_len, "stage holders disagree on seq_len");
        for layer in 0..n_layers {
            if (start..end).contains(&layer) {
                merged.k_data[layer].clone_from(&holder.k_data[layer]);
                merged.v_data[layer].clone_from(&holder.v_data[layer]);
            } else {
                assert!(
                    holder.k_data[layer].is_empty() && holder.v_data[layer].is_empty(),
                    "stage [{start}, {end}) wrote layer {layer}, which it does not hold"
                );
            }
        }
        start = end;
    }
    merged
}

/// The worker's generation (`try_generate`) replayed through stage holders,
/// as the sharded verifier replays it: the terminal stage selects each token
/// with the generated history.
fn generate_through_stages(
    model: &CachedIntegerModel,
    ends: &[usize],
    prompt: &[u32],
    max_tokens: usize,
) -> (Vec<u32>, StageTrace) {
    let mut holders = stage_holders(model, ends);
    let mut trace = StageTrace::default();
    let mut fed = vec![model.config.bos_token];
    fed.extend_from_slice(prompt);
    for (position, &token) in fed.iter().enumerate() {
        stage_step(model, ends, &mut holders, position, token, &[], &mut trace);
    }
    let mut generated: Vec<u32> = Vec::with_capacity(max_tokens);
    let mut last = *prompt.last().expect("a non-empty prompt");
    for position in fed.len()..fed.len() + max_tokens {
        let next = stage_step(
            model,
            ends,
            &mut holders,
            position,
            last,
            &generated,
            &mut trace,
        );
        generated.push(next);
        last = next;
    }
    (generated, trace)
}

// ── Sequences ───────────────────────────────────────────────────────────────

/// A sequence that fills the KAT model's whole context window and starts with
/// the KAT sequence, so the reference's first rows are reviewed constants.
fn full_window(fixture: &GoldenFixture) -> Vec<u32> {
    let mut tokens = fixture.sequence_tokens.clone();
    tokens.extend_from_slice(&[2, 19, 22, 0, 17, 4, 9, 21, 6, 15]);
    assert_eq!(tokens.len(), fixture.max_seq);
    tokens
}

/// Pins the reference's first rows to the reviewed KAT constants: logits
/// hashes, next tokens and KV hash, and for the live profile the hidden state
/// after layers 0 and 2 that the KAT's three-way split committed.
fn pin_reference(fixture: &GoldenFixture, profile: Profile, reference: &Reference) {
    let pinned = fixture.sequence_tokens.len();
    let (next_tokens, logits_hashes, kv_hash) = match profile {
        Profile::LegacySplitHalf => (
            fixture.expected.next_tokens.clone(),
            fixture.expected.logits_hashes.clone(),
            fixture.expected.kv_cache_hash.clone(),
        ),
        Profile::GgufInterleaved => {
            let doc = operators();
            let section = &doc["interleaved_generation_v2"];
            assert_eq!(
                token_ids(&section["sequence_tokens"]),
                fixture.sequence_tokens
            );
            let hashes: Vec<String> = section["logits_hashes"]
                .as_array()
                .expect("logits_hashes")
                .iter()
                .map(|hash| text(hash).to_owned())
                .collect();
            (
                token_ids(&section["next_tokens"]),
                hashes,
                text(&section["kv_cache_hash_split_half_layout"]).to_owned(),
            )
        }
    };
    for position in 0..pinned {
        let logits = &reference.trace.logits[&position];
        assert_eq!(
            hash_i64(logits),
            logits_hashes[position],
            "{profile:?}: reference logits at position {position} drifted from the KAT"
        );
        assert_eq!(
            select(logits, &[]),
            next_tokens[position],
            "{profile:?}: reference next token at position {position} drifted from the KAT"
        );
    }
    let mut prefix = clone_kv(&reference.kv);
    truncate_kv(&mut prefix, pinned, reference.d_kv);
    assert_eq!(
        hash_cache(&prefix),
        kv_hash,
        "{profile:?}: reference KV drifted from the KAT"
    );
    if profile == Profile::LegacySplitHalf {
        assert_eq!(fixture.shard_boundaries, [1, 3]);
        for position in 0..pinned {
            for (stage, layer) in [0usize, 2].into_iter().enumerate() {
                assert_eq!(
                    reference.trace.boundaries[&(position, layer)],
                    fixture.expected.shard_hidden_hashes[2 * position + stage],
                    "reference boundary after layer {layer} at position {position} drifted from the KAT"
                );
            }
        }
    }
}

/// Reviewed constants for what the KAT files leave out: every position of the
/// full-window references, the wide model, and the two generation calls the
/// KATs do not cover. Each value comes from the token-by-token reference run
/// (scalar kernel, one thread) and was byte-identical on all four golden
/// runners of one CI run. A value that differed across runners would be a
/// finding, not a pin.
///
/// Provenance: run 37965430159 on commit 44dc3cf6 (#179's 7d7fd472 plus a
/// scratch printer), jobs 113938505754 (ubuntu-latest), 113938505688
/// (windows-latest), 113938505276 (macos-15-intel) and 113938505553
/// (macos-15). The same run also reproduced every existing KAT constant.
const REFERENCE_JSON: &str = include_str!("../../tests/fixtures/execution_modes_reference.json");

fn pinned() -> serde_json::Value {
    let document: serde_json::Value =
        serde_json::from_str(REFERENCE_JSON).expect("the pinned reference must be valid JSON");
    assert_eq!(document["schema"], 1, "unsupported pinned reference schema");
    document
}

/// Pins every row of a token-by-token reference to the reviewed constants:
/// the boundary leaving each layer, then the logits, position by position,
/// then the KV hash.
fn pin_rows(section: &serde_json::Value, tokens: &[u32], reference: &Reference, name: &str) {
    assert_eq!(
        token_ids(&section["tokens"]),
        tokens,
        "{name}: pinned tokens"
    );
    let logits = section["logits_hashes"].as_array().expect("logits_hashes");
    let boundaries = section["boundary_hashes"]
        .as_array()
        .expect("boundary_hashes");
    assert_eq!(
        (logits.len(), boundaries.len()),
        (tokens.len(), tokens.len()),
        "{name}: pinned row count"
    );
    for (position, (logits_hash, layers)) in logits.iter().zip(boundaries).enumerate() {
        let layers = layers.as_array().expect("one boundary hash per layer");
        assert_eq!(layers.len(), reference.n_layers(), "{name}: pinned layers");
        for (layer, hash) in layers.iter().enumerate() {
            assert_eq!(
                reference.trace.boundaries[&(position, layer)],
                text(hash),
                "{name}: boundary at position {position}, layer {layer} drifted from the pinned reference"
            );
        }
        assert_eq!(
            hash_i64(&reference.trace.logits[&position]),
            text(logits_hash),
            "{name}: logits at position {position} drifted from the pinned reference"
        );
    }
    assert_eq!(
        hash_cache(&reference.kv),
        text(&section["kv_cache_hash"]),
        "{name}: KV drifted from the pinned reference"
    );
}

/// The KAT recipe at a width where the gate, up and output projections each
/// span two 256-row rayon tasks, so thread counts split projection rows too.
fn wide_fixture() -> GoldenFixture {
    let mut wide = fixture();
    wide.vocab_size = 260;
    wide.d_ff = 264;
    wide.n_layers = 3;
    wide.max_seq = 40;
    wide
}

fn wide_tokens(fixture: &GoldenFixture) -> Vec<u32> {
    let vocab = u32::try_from(fixture.vocab_size).expect("vocabulary fits u32");
    let len = u32::try_from(fixture.max_seq).expect("context fits u32");
    (0..len).map(|i| (i * 37 + 11) % vocab).collect()
}

/// Runs every schedule on every leg and holds it to the base-leg reference.
fn sweep_schedules(
    switch: &KernelSwitch,
    model: &CachedIntegerModel,
    tokens: &[u32],
    reference: &Reference,
    schedules: &[Schedule],
    name: &str,
) {
    let len = tokens.len();
    for leg in switch.legs() {
        for &schedule in schedules {
            if leg == BASE_LEG && matches!(schedule, Schedule::TokenByToken) {
                // This cell is the reference itself; comparing it with
                // itself proves nothing, so it is not a covered mode.
                continue;
            }
            let mode = format!("{name}, {leg:?}: {schedule:?} over {len} positions");
            let (trace, kv) = switch.run(leg, || run_fresh(model, schedule, tokens));
            let logits_from = match schedule {
                Schedule::LastOnly(_) => len - 1,
                Schedule::TokenByToken | Schedule::Chunked(_) => 0,
            };
            reference.assert_rows(&trace, 0..len, logits_from, &mode);
            reference.assert_kv(&kv, len, &mode);
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Modes 1, 2, 3 and 7: token by token, one pass, every chunk size and the
/// serving shape, on every leg, against a reference pinned to the KAT.
#[test]
fn golden_modes_one_pass_and_chunked_prefill_match_token_by_token() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let tokens = full_window(&fixture);
    let len = tokens.len();
    let mut schedules = vec![Schedule::TokenByToken, Schedule::Chunked(len)];
    schedules.extend([1, 2, 3, 4, 5, 7, 8, len - 1].map(Schedule::Chunked));
    schedules.extend([Schedule::LastOnly(len), Schedule::LastOnly(5)]);
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let reference = switch.run(BASE_LEG, || Reference::new(&model, &tokens));
        pin_reference(&fixture, profile, &reference);
        pin_rows(
            &pinned()["full_window"][format!("{profile:?}").as_str()],
            &tokens,
            &reference,
            &format!("{profile:?} full window"),
        );
        sweep_schedules(
            &switch,
            &model,
            &tokens,
            &reference,
            &schedules,
            &format!("{profile:?}"),
        );
    }

    let wide = wide_fixture();
    let tokens = wide_tokens(&wide);
    let len = tokens.len();
    let model = build_model(&wide, Profile::LegacySplitHalf);
    let reference = switch.run(BASE_LEG, || Reference::new(&model, &tokens));
    pin_rows(
        &pinned()["wide"]["LegacySplitHalf"],
        &tokens,
        &reference,
        "wide LegacySplitHalf",
    );
    let schedules = [
        Schedule::TokenByToken,
        Schedule::Chunked(len),
        Schedule::Chunked(3),
        Schedule::Chunked(16),
        Schedule::LastOnly(7),
    ];
    sweep_schedules(
        &switch,
        &model,
        &tokens,
        &reference,
        &schedules,
        "wide LegacySplitHalf",
    );
}

/// Mode 2 as a validator uses it: one teacher-forced pass over the prompt and
/// a finished generation re-derives every generated token and the output hash
/// of both production generation calls, with token-by-token rows and KV.
#[test]
fn golden_modes_teacher_forced_recheck_reproduces_the_generation() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let (prompt, max_tokens) = generation_case(&fixture, profile);
        for generation in GENERATIONS {
            let (generated, hash) = switch.run(BASE_LEG, || {
                production_generate(&model, generation, &prompt, max_tokens)
            });
            pin_generation(&fixture, profile, generation, &generated, &hash);
            let (fed, first_row) = fed_sequence(&model, generation, &prompt, &generated);
            let reference = switch.run(BASE_LEG, || Reference::new(&model, &fed));
            assert_eq!(
                recheck(&reference.trace, first_row, generated.len()),
                generated,
                "{profile:?} {generation:?}: the token-by-token replay must select the generation"
            );
            for leg in switch.legs() {
                let again = switch.run(leg, || {
                    production_generate(&model, generation, &prompt, max_tokens)
                });
                assert_eq!(
                    again,
                    (generated.clone(), hash.clone()),
                    "{profile:?} {generation:?}, {leg:?}: production generation"
                );
                for chunk in [fed.len(), 4] {
                    let mode = format!(
                        "{profile:?}, {leg:?}: teacher-forced {generation:?} re-check in chunks of {chunk}"
                    );
                    let (trace, kv) =
                        switch.run(leg, || run_fresh(&model, Schedule::Chunked(chunk), &fed));
                    reference.assert_rows(&trace, 0..fed.len(), 0, &mode);
                    reference.assert_kv(&kv, fed.len(), &mode);
                    let rechecked = recheck(&trace, first_row, generated.len());
                    assert_eq!(rechecked, generated, "{mode}: re-derived tokens");
                    assert_eq!(
                        output_hash(&rechecked),
                        hash,
                        "{mode}: re-derived output hash"
                    );
                }
            }
        }
    }
}

/// Mode 4: a prefix computed on one node, by either schedule, is shipped as
/// bytes, restored on a separately built model instance on every leg, and
/// resumed by every schedule.
#[test]
fn golden_modes_restored_prefix_resumes_bit_identically() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let tokens = full_window(&fixture);
    let len = tokens.len();
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let reference = switch.run(BASE_LEG, || Reference::new(&model, &tokens));
        for split in [1, 6, 11, len - 1] {
            for prefix_schedule in [Schedule::TokenByToken, Schedule::Chunked(split)] {
                let (_, prefix) = switch.run(BASE_LEG, || {
                    run_fresh(&model, prefix_schedule, &tokens[..split])
                });
                let image = kv_to_bytes(&prefix);
                let restored = kv_from_bytes(&image);
                assert_eq!(kv_to_bytes(&restored), image, "the KV image round-trips");
                assert_eq!(hash_cache(&restored), hash_cache(&prefix));
                reference.assert_kv(
                    &restored,
                    split,
                    &format!("{profile:?}: restored prefix of {split} by {prefix_schedule:?}"),
                );
                let suffix_schedules = [
                    Schedule::TokenByToken,
                    Schedule::Chunked(len - split),
                    Schedule::Chunked(3),
                ];
                for leg in switch.legs() {
                    let node_b = build_model(&fixture, profile);
                    for suffix_schedule in suffix_schedules {
                        let mode = format!(
                            "{profile:?}, {leg:?}: prefix of {split} by {prefix_schedule:?} \
                             restored from bytes, suffix by {suffix_schedule:?}"
                        );
                        let (trace, kv) = switch.run(leg, || {
                            let mut kv = kv_from_bytes(&image);
                            let mut trace = Trace::default();
                            feed(
                                &node_b,
                                suffix_schedule,
                                &tokens[split..],
                                &mut kv,
                                &mut trace,
                            );
                            (trace, kv)
                        });
                        reference.assert_rows(&trace, split..len, split, &mode);
                        reference.assert_kv(&kv, len, &mode);
                    }
                }
            }
        }
    }
}

/// Mode 5: speculative decoding with every draft pattern and several k
/// reproduces both production generation calls, keeps exactly the
/// token-by-token rows, and leaves the token-by-token KV after rollback.
#[test]
fn golden_modes_speculative_verification_matches_incremental_decode() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let (prompt, max_tokens) = generation_case(&fixture, profile);
        for generation in GENERATIONS {
            let (truth, truth_hash) = switch.run(BASE_LEG, || {
                production_generate(&model, generation, &prompt, max_tokens)
            });
            pin_generation(&fixture, profile, generation, &truth, &truth_hash);
            let (fed, _) = fed_sequence(&model, generation, &prompt, &truth);
            let reference = switch.run(BASE_LEG, || Reference::new(&model, &fed));
            for leg in switch.legs() {
                for k in [1, 2, 4, truth.len()] {
                    for drafts in DRAFT_PATTERNS {
                        let mode = format!(
                            "{profile:?}, {leg:?}: speculative {generation:?}, k={k}, {drafts:?}"
                        );
                        let run = switch.run(leg, || {
                            speculate(&model, generation, &prompt, &truth, drafts, k)
                        });
                        assert_eq!(run.tokens, truth, "{mode}: tokens");
                        assert_eq!(output_hash(&run.tokens), truth_hash, "{mode}: output hash");
                        reference.assert_rows(&run.trace, 0..fed.len(), 0, &mode);
                        reference.assert_kv(&run.kv, fed.len(), &mode);
                        // Every case must do exactly what its pattern plans,
                        // and every pattern but AllRight must reject and roll
                        // back, so no case quietly repeats the all-right one.
                        let plan = planned_speculation(generation, truth.len(), drafts, k);
                        assert_eq!(run.counts, plan, "{mode}: drafts accepted and rolled back");
                        match drafts {
                            Drafts::AllRight => assert!(
                                plan.rejected_rounds == 0 && plan.accepted_drafts > 0,
                                "{mode}: the all-right case must accept and never reject"
                            ),
                            Drafts::AllWrong => assert!(
                                plan.accepted_drafts == 0 && plan.rejected_rounds > 0,
                                "{mode}: the all-wrong case must reject every round"
                            ),
                            Drafts::LastWrong | Drafts::MiddleWrongEvenRounds => assert!(
                                plan.rejected_rounds > 0 && plan.rolled_back_rows > 0,
                                "{mode}: the case never rejected a draft"
                            ),
                        }
                        if leg == BASE_LEG {
                            println!("golden_modes speculative case: {mode}: {:?}", run.counts);
                        }
                    }
                }
            }
        }
    }
}

/// Mode 6: every 1-, 2-, 3- and 4-way stage split, one KV cache per stage
/// holder, hands on the token-by-token boundary at every cut and commits the
/// token-by-token logits; the worker's generation replayed through every split
/// reproduces the worker; and stage holders resume from KV restored from bytes.
#[test]
fn golden_modes_every_stage_split_matches_token_by_token() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let tokens = full_window(&fixture);
    let len = tokens.len();
    let splits = stage_splits(fixture.n_layers);
    let expected_splits: [&[usize]; 8] = [
        &[4],
        &[1, 4],
        &[2, 4],
        &[1, 2, 4],
        &[3, 4],
        &[1, 3, 4],
        &[2, 3, 4],
        &[1, 2, 3, 4],
    ];
    assert_eq!(splits, expected_splits);
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let reference = switch.run(BASE_LEG, || Reference::new(&model, &tokens));
        let (prompt, max_tokens) = generation_case(&fixture, profile);
        let (truth, truth_hash) = switch.run(BASE_LEG, || {
            production_generate(&model, Generation::Worker, &prompt, max_tokens)
        });
        let (fed, first_row) = fed_sequence(&model, Generation::Worker, &prompt, &truth);
        let worker_reference = switch.run(BASE_LEG, || Reference::new(&model, &fed));
        let worker_history = |position: usize| {
            let selected = position.saturating_sub(first_row).min(truth.len());
            truth[..selected].to_vec()
        };
        for leg in switch.legs() {
            for ends in &splits {
                let mode = format!(
                    "{profile:?}, {leg:?}: {}-way stage split {ends:?}",
                    ends.len()
                );
                let (trace, holders) = switch.run(leg, || {
                    let mut holders = stage_holders(&model, ends);
                    let mut trace = StageTrace::default();
                    for (position, &token) in tokens.iter().enumerate() {
                        stage_step(&model, ends, &mut holders, position, token, &[], &mut trace);
                    }
                    (trace, holders)
                });
                reference.assert_stages(&trace, ends, 0..len, |_| Vec::new(), &mode);
                reference.assert_kv(&merge_holders(ends, &holders), len, &mode);

                let mode = format!("{mode}, worker generation");
                let (generated, trace) = switch.run(leg, || {
                    generate_through_stages(&model, ends, &prompt, truth.len())
                });
                assert_eq!(generated, truth, "{mode}: tokens");
                assert_eq!(output_hash(&generated), truth_hash, "{mode}: output hash");
                worker_reference.assert_stages(&trace, ends, 0..fed.len(), worker_history, &mode);
            }

            // Stage holders checkpoint mid-sequence: each holder's KV goes
            // through bytes and resumes on a separately built model instance.
            let ends = [1, 3, 4];
            for split in [5, 12] {
                let mode = format!(
                    "{profile:?}, {leg:?}: stage split {ends:?} resumed at {split} from bytes"
                );
                let images: Vec<Vec<u8>> = switch.run(BASE_LEG, || {
                    let mut holders = stage_holders(&model, &ends);
                    let mut trace = StageTrace::default();
                    for (position, &token) in tokens[..split].iter().enumerate() {
                        stage_step(
                            &model,
                            &ends,
                            &mut holders,
                            position,
                            token,
                            &[],
                            &mut trace,
                        );
                    }
                    holders.iter().map(kv_to_bytes).collect()
                });
                let node_b = build_model(&fixture, profile);
                let (trace, holders) = switch.run(leg, || {
                    let mut holders: Vec<KVCache> = images
                        .iter()
                        .map(Vec::as_slice)
                        .map(kv_from_bytes)
                        .collect();
                    let mut trace = StageTrace::default();
                    for (position, &token) in tokens.iter().enumerate().skip(split) {
                        stage_step(
                            &node_b,
                            &ends,
                            &mut holders,
                            position,
                            token,
                            &[],
                            &mut trace,
                        );
                    }
                    (trace, holders)
                });
                reference.assert_stages(&trace, &ends, split..len, |_| Vec::new(), &mode);
                reference.assert_kv(&merge_holders(&ends, &holders), len, &mode);
            }
        }
    }
}

/// TODO(seeded sampling): pending an engine API.
///
/// The engine selects tokens only by greedy argmax and ARC's deterministic
/// repetition penalty, both functions of (logits, generated history) alone,
/// and the teacher-forced test above already re-derives them from one pass.
/// There is no stochastic sampler, so there is no seed to re-check yet. A
/// sampler must draw its randomness from (request seed, position) only, for
/// example a domain-separated BLAKE3 output over both, and build its
/// probabilities from `integer_exp`, never from floats. Then replace this body
/// with: sample a generation on the base leg; re-derive every sampled token
/// from teacher-forced logits plus (seed, position) on every leg and in every
/// mode above; require the same tokens and output hash; and require that a
/// different seed changes at least one token.
#[test]
#[ignore = "TODO: the engine has no seeded sampler yet; see the doc comment"]
fn golden_modes_seeded_sampling_rechecks_from_teacher_forced_logits() {
    panic!("not implemented: the engine has no seeded sampler to re-check");
}

// ── Multi-row stage calls ───────────────────────────────────────────────────

/// Feeds `tokens` at positions `position..` through every stage holder with
/// one `forward_shard_rows` call per stage. Records the hidden rows handed
/// across every cut and returns the last stage's raw logits rows.
fn stage_rows_step(
    model: &CachedIntegerModel,
    ends: &[usize],
    holders: &mut [KVCache],
    position: usize,
    tokens: &[u32],
    trace: &mut StageTrace,
) -> Vec<Vec<i64>> {
    assert_eq!(ends.len(), holders.len());
    let mut input = ShardRowsInput::Tokens(tokens.to_vec());
    let mut start = 0;
    for (&end, holder) in ends.iter().zip(holders.iter_mut()) {
        let output = model
            .forward_shard_rows(input, holder, start, end, position)
            .unwrap_or_else(|error| {
                panic!(
                    "stage [{start}, {end}) refused {} rows at position {position}: {error}",
                    tokens.len()
                )
            });
        match output {
            ShardRowsOutput::Hidden(rows) => {
                for (offset, row) in rows.iter().enumerate() {
                    let previous = trace
                        .boundaries
                        .insert((position + offset, end - 1), hash_i64(row));
                    assert!(previous.is_none(), "stage boundary {end} reported twice");
                }
                input = ShardRowsInput::Hidden(rows);
            }
            ShardRowsOutput::Logits(rows) => {
                assert_eq!(end, model.config.n_layers, "only the last stage has logits");
                assert_eq!(rows.len(), tokens.len(), "one logits row per input row");
                return rows;
            }
        }
        start = end;
    }
    panic!("the last stage of {ends:?} returned hidden rows, not logits");
}

/// Records what a one-row terminal stage would commit for `logits` rows at
/// positions `position..`: each row's token and penalized logits hash,
/// selected with that row's own generated history.
fn record_terminal(
    trace: &mut StageTrace,
    position: usize,
    logits: &[Vec<i64>],
    history: impl Fn(usize) -> Vec<u32>,
) {
    for (offset, row) in logits.iter().enumerate() {
        let selected = history(position + offset);
        let commitment = (select(row, &selected), penalized_hash(row, &selected));
        let previous = trace.terminal.insert(position + offset, commitment);
        assert!(
            previous.is_none(),
            "position {} committed twice",
            position + offset
        );
    }
}

impl Reference {
    /// Holds raw logits rows at positions `position..` to the reference.
    fn assert_logit_rows(&self, position: usize, rows: &[Vec<i64>], mode: &str) {
        for (offset, row) in rows.iter().enumerate() {
            let expected = &self.trace.logits[&(position + offset)];
            assert!(
                row == expected,
                "{mode} DIFFERS from token by token: logits at position {} ({} vs {})",
                position + offset,
                hash_i64(row),
                hash_i64(expected)
            );
        }
    }
}

/// The generated history a row at `position` selects with: the tokens
/// generated before it. Rows before `first_row` select nothing.
fn generated_history(truth: &[u32], first_row: usize, position: usize) -> Vec<u32> {
    truth[..position.saturating_sub(first_row).min(truth.len())].to_vec()
}

/// Multi-row stage calls: every 1-, 2-, 3- and 4-way split runs the whole
/// window k rows per call on every stage, for k of 1, 2, 3, 4 and 8, and must
/// hand on exactly the token-by-token boundaries, logits and K/V rows.
#[test]
fn golden_modes_stage_holders_verify_rows_in_one_pass() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let tokens = full_window(&fixture);
    let len = tokens.len();
    let splits = stage_splits(fixture.n_layers);
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let reference = switch.run(BASE_LEG, || Reference::new(&model, &tokens));
        for leg in switch.legs() {
            for ends in &splits {
                for k in [1, 2, 3, 4, 8] {
                    let mode = format!(
                        "{profile:?}, {leg:?}: {}-way split {ends:?}, {k} rows per stage call",
                        ends.len()
                    );
                    let (trace, holders) = switch.run(leg, || {
                        let mut holders = stage_holders(&model, ends);
                        let mut trace = StageTrace::default();
                        for (index, chunk) in tokens.chunks(k).enumerate() {
                            let position = index * k;
                            let logits = stage_rows_step(
                                &model,
                                ends,
                                &mut holders,
                                position,
                                chunk,
                                &mut trace,
                            );
                            reference.assert_logit_rows(position, &logits, &mode);
                            record_terminal(&mut trace, position, &logits, |_| Vec::new());
                        }
                        (trace, holders)
                    });
                    reference.assert_stages(&trace, ends, 0..len, |_| Vec::new(), &mode);
                    reference.assert_kv(&merge_holders(ends, &holders), len, &mode);
                }
            }
        }
    }
}

/// What a speculative decode through stage holders produced.
struct StageSpeculation {
    tokens: Vec<u32>,
    trace: StageTrace,
    holders: Vec<KVCache>,
    counts: SpeculationCounts,
}

/// A speculative decode through stage holders. Each round runs the pending
/// token and its drafts through every stage in one multi-row call per stage,
/// selects each row's token with that row's history, and rolls every holder
/// back to the last accepted row with `rollback_rows`.
fn speculate_through_stages(
    model: &CachedIntegerModel,
    ends: &[usize],
    generation: Generation,
    prompt: &[u32],
    truth: &[u32],
    drafts: Drafts,
    k: usize,
) -> StageSpeculation {
    let config = &model.config;
    let vocab = u32::try_from(config.vocab_size).expect("vocabulary fits u32");
    let max_tokens = truth.len();
    let mut holders = stage_holders(model, ends);
    let mut trace = StageTrace::default();
    let mut prompt_rows = vec![config.bos_token];
    prompt_rows.extend_from_slice(prompt);
    let prompt_logits = stage_rows_step(model, ends, &mut holders, 0, &prompt_rows, &mut trace);
    record_terminal(&mut trace, 0, &prompt_logits, |_| Vec::new());

    let mut tokens: Vec<u32> = Vec::with_capacity(max_tokens);
    let mut pending = match generation {
        Generation::Worker => *prompt.last().expect("a non-empty prompt"),
        Generation::V2 => {
            let first = select(&prompt_logits[prompt.len()], &tokens);
            tokens.push(first);
            first
        }
    };
    let mut counts = SpeculationCounts::default();
    let mut round = 0;
    while tokens.len() < max_tokens {
        let n_drafts = k.min(max_tokens - tokens.len() - 1);
        let first = tokens.len();
        let mut batch = vec![pending];
        batch.extend((0..n_drafts).map(|slot| {
            let right = truth[first + slot];
            if drafts.wrong(round, slot, n_drafts) {
                (right + 1) % vocab
            } else {
                right
            }
        }));
        let base = holders[0].seq_len;
        let mut verified = StageTrace::default();
        let logits = stage_rows_step(model, ends, &mut holders, base, &batch, &mut verified);

        // Row `j` selects what follows `batch[j]`, with every token accepted
        // before it as its history, exactly as the one-row terminal stage does.
        let mut kept = 1;
        let mut next = select(&logits[0], &tokens);
        let mut committed = vec![(next, penalized_hash(&logits[0], &tokens))];
        while kept < batch.len() && batch[kept] == next {
            tokens.push(next);
            next = select(&logits[kept], &tokens);
            committed.push((next, penalized_hash(&logits[kept], &tokens)));
            kept += 1;
        }
        counts.accepted_drafts += kept - 1;
        if kept < batch.len() {
            counts.rejected_rounds += 1;
            counts.rolled_back_rows += batch.len() - kept;
            for holder in &mut holders {
                model
                    .rollback_rows(holder, base + kept)
                    .expect("rolling back to an earlier position is accepted");
            }
        }
        for ((position, layer), hash) in verified.boundaries {
            if position < base + kept {
                let previous = trace.boundaries.insert((position, layer), hash);
                assert!(previous.is_none(), "position {position} kept twice");
            }
        }
        for (offset, commitment) in committed.into_iter().enumerate() {
            let previous = trace.terminal.insert(base + offset, commitment);
            assert!(
                previous.is_none(),
                "position {} committed twice",
                base + offset
            );
        }
        tokens.push(next);
        pending = next;
        round += 1;
    }
    if matches!(generation, Generation::V2) {
        // generate_v2 feeds every generated token back, the last one included.
        let base = holders[0].seq_len;
        let logits = stage_rows_step(model, ends, &mut holders, base, &[pending], &mut trace);
        let history = tokens.clone();
        record_terminal(&mut trace, base, &logits, |_| history.clone());
    }
    StageSpeculation {
        tokens,
        trace,
        holders,
        counts,
    }
}

/// Partial acceptance through stage holders: drafts verified k rows per stage
/// call, rejected rows rolled back on every holder, decoding continued. Each
/// case must reproduce the production generation, commit the token-by-token
/// boundaries and terminal outputs, end on the token-by-token KV, and reject
/// and roll back exactly what its draft pattern plans, at least once.
#[test]
fn golden_modes_stage_holders_roll_back_rejected_rows() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let splits: [&[usize]; 4] = [&[4], &[2, 4], &[1, 3, 4], &[1, 2, 3, 4]];
    let patterns = [
        Drafts::AllWrong,
        Drafts::LastWrong,
        Drafts::MiddleWrongEvenRounds,
    ];
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let (prompt, max_tokens) = generation_case(&fixture, profile);
        for generation in GENERATIONS {
            let (truth, truth_hash) = switch.run(BASE_LEG, || {
                production_generate(&model, generation, &prompt, max_tokens)
            });
            let (fed, first_row) = fed_sequence(&model, generation, &prompt, &truth);
            let reference = switch.run(BASE_LEG, || Reference::new(&model, &fed));
            for leg in switch.legs() {
                for ends in splits {
                    for k in [1, 3, 8] {
                        for drafts in patterns {
                            let mode = format!(
                                "{profile:?}, {leg:?}: {generation:?} speculated through split \
                                 {ends:?}, k={k}, {drafts:?}"
                            );
                            let run = switch.run(leg, || {
                                speculate_through_stages(
                                    &model, ends, generation, &prompt, &truth, drafts, k,
                                )
                            });
                            assert_eq!(run.tokens, truth, "{mode}: tokens");
                            assert_eq!(output_hash(&run.tokens), truth_hash, "{mode}: output hash");
                            let plan = planned_speculation(generation, truth.len(), drafts, k);
                            assert_eq!(run.counts, plan, "{mode}: drafts accepted and rolled back");
                            assert!(
                                plan.rejected_rounds > 0 && plan.rolled_back_rows > 0,
                                "{mode}: the case never rejected a draft"
                            );
                            reference.assert_stages(
                                &run.trace,
                                ends,
                                0..fed.len(),
                                |position| generated_history(&truth, first_row, position),
                                &mode,
                            );
                            reference.assert_kv(
                                &merge_holders(ends, &run.holders),
                                fed.len(),
                                &mode,
                            );
                            if leg == BASE_LEG {
                                println!(
                                    "golden_modes stage speculation case: {mode}: {:?}",
                                    run.counts
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One-row and multi-row calls interleaved on the same stage holders, then a
/// rollback to mid-window and the rest fed again in one multi-row call: every
/// boundary, terminal output and K/V row must stay token-by-token.
#[test]
fn golden_modes_stage_holders_mix_one_row_and_multi_row_calls() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let tokens = full_window(&fixture);
    let len = tokens.len();
    // A segment of 1 goes through forward_shard_token, a longer one through
    // forward_shard_rows.
    let segments: [usize; 7] = [1, 3, 1, 4, 2, 1, 4];
    assert_eq!(segments.iter().sum::<usize>(), len);
    let splits: [&[usize]; 3] = [&[2, 4], &[1, 3, 4], &[1, 2, 3, 4]];
    let resume = 9;
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let reference = switch.run(BASE_LEG, || Reference::new(&model, &tokens));
        for leg in switch.legs() {
            for ends in splits {
                let mode = format!(
                    "{profile:?}, {leg:?}: split {ends:?}, mixed one-row and multi-row calls"
                );
                let (trace, again, holders) = switch.run(leg, || {
                    let mut holders = stage_holders(&model, ends);
                    let mut trace = StageTrace::default();
                    let mut position = 0;
                    for &size in &segments {
                        let chunk = &tokens[position..position + size];
                        if size == 1 {
                            stage_step(
                                &model,
                                ends,
                                &mut holders,
                                position,
                                chunk[0],
                                &[],
                                &mut trace,
                            );
                        } else {
                            let logits = stage_rows_step(
                                &model,
                                ends,
                                &mut holders,
                                position,
                                chunk,
                                &mut trace,
                            );
                            record_terminal(&mut trace, position, &logits, |_| Vec::new());
                        }
                        position += size;
                    }
                    for holder in &mut holders {
                        model
                            .rollback_rows(holder, resume)
                            .expect("rolling back mid-window is accepted");
                    }
                    let mut again = StageTrace::default();
                    let logits = stage_rows_step(
                        &model,
                        ends,
                        &mut holders,
                        resume,
                        &tokens[resume..],
                        &mut again,
                    );
                    record_terminal(&mut again, resume, &logits, |_| Vec::new());
                    (trace, again, holders)
                });
                reference.assert_stages(&trace, ends, 0..len, |_| Vec::new(), &mode);
                let mode = format!("{mode}, rolled back to {resume} and fed again");
                reference.assert_stages(&again, ends, resume..len, |_| Vec::new(), &mode);
                reference.assert_kv(&merge_holders(ends, &holders), len, &mode);
            }
        }
    }
}

/// A teacher-forced re-check through stage holders whose multi-row calls
/// cross the prompt/generation boundary: prompt rows select nothing, and each
/// generation row selects with its own history. The re-derived tokens, the
/// terminal commitments, the boundaries and the K/V rows must all match.
#[test]
fn golden_modes_stage_holders_recheck_rows_across_the_prompt_boundary() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let splits = stage_splits(fixture.n_layers);
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let (prompt, max_tokens) = generation_case(&fixture, profile);
        for generation in GENERATIONS {
            let (truth, truth_hash) = switch.run(BASE_LEG, || {
                production_generate(&model, generation, &prompt, max_tokens)
            });
            let (fed, first_row) = fed_sequence(&model, generation, &prompt, &truth);
            let reference = switch.run(BASE_LEG, || Reference::new(&model, &fed));
            let history = |position: usize| generated_history(&truth, first_row, position);
            for leg in switch.legs() {
                for ends in &splits {
                    for k in [2, 3, 4, 8] {
                        let mode = format!(
                            "{profile:?}, {leg:?}: {generation:?} re-checked through split \
                             {ends:?}, {k} rows per call across the prompt boundary"
                        );
                        // One call up to the row before the first selecting row,
                        // then k rows per call, so the first k-row call holds a
                        // prompt row and the first generation row together.
                        let lead = first_row - 1;
                        let (trace, holders) = switch.run(leg, || {
                            let mut holders = stage_holders(&model, ends);
                            let mut trace = StageTrace::default();
                            let logits = stage_rows_step(
                                &model,
                                ends,
                                &mut holders,
                                0,
                                &fed[..lead],
                                &mut trace,
                            );
                            record_terminal(&mut trace, 0, &logits, history);
                            let mut position = lead;
                            for chunk in fed[lead..].chunks(k) {
                                let logits = stage_rows_step(
                                    &model,
                                    ends,
                                    &mut holders,
                                    position,
                                    chunk,
                                    &mut trace,
                                );
                                record_terminal(&mut trace, position, &logits, history);
                                position += chunk.len();
                            }
                            (trace, holders)
                        });
                        let rechecked: Vec<u32> = (first_row..first_row + truth.len())
                            .map(|position| trace.terminal[&position].0)
                            .collect();
                        assert_eq!(rechecked, truth, "{mode}: re-derived tokens");
                        assert_eq!(
                            output_hash(&rechecked),
                            truth_hash,
                            "{mode}: re-derived output hash"
                        );
                        reference.assert_stages(&trace, ends, 0..fed.len(), history, &mode);
                        reference.assert_kv(&merge_holders(ends, &holders), fed.len(), &mode);
                    }
                }
            }
        }
    }
}

/// Every refusal of `forward_shard_rows` and `rollback_rows` leaves the cache
/// byte for byte as it was.
#[test]
fn golden_modes_stage_rows_refuse_without_touching_the_cache() {
    let fixture = fixture();
    let model = build_model(&fixture, Profile::LegacySplitHalf);
    let n_layers = model.config.n_layers;
    let d = model.config.d_model;
    let mut holder = KVCache::new(n_layers);
    model
        .forward_shard_rows(ShardRowsInput::Tokens(vec![1, 3]), &mut holder, 0, 2, 0)
        .expect("two rows on the first stage");
    let before = kv_to_bytes(&holder);
    let refuse =
        |holder: &mut KVCache, input: ShardRowsInput, start: usize, end: usize, position: usize| {
            model
                .forward_shard_rows(input, holder, start, end, position)
                .expect_err("the call must be refused")
        };
    assert_eq!(
        refuse(&mut holder, ShardRowsInput::Tokens(Vec::new()), 0, 2, 2),
        ShardRowsError::NoRows
    );
    assert_eq!(
        refuse(&mut holder, ShardRowsInput::Tokens(vec![7]), 2, 2, 2).kind(),
        "bad_layer_range"
    );
    assert_eq!(
        refuse(
            &mut holder,
            ShardRowsInput::Tokens(vec![7]),
            0,
            n_layers + 1,
            2
        )
        .kind(),
        "bad_layer_range"
    );
    assert_eq!(
        refuse(
            &mut holder,
            ShardRowsInput::Hidden(vec![vec![0; d]]),
            0,
            2,
            2
        ),
        ShardRowsError::WrongInput { start_layer: 0 }
    );
    assert_eq!(
        refuse(&mut holder, ShardRowsInput::Tokens(vec![7]), 0, 2, 1).kind(),
        "kv_cache_out_of_sync"
    );
    assert_eq!(
        refuse(&mut holder, ShardRowsInput::Tokens(vec![7; 15]), 0, 2, 2).kind(),
        "position_out_of_range"
    );
    assert_eq!(
        kv_to_bytes(&holder),
        before,
        "a refused call changed the cache"
    );

    let mut second = KVCache::new(n_layers);
    assert_eq!(
        refuse(&mut second, ShardRowsInput::Tokens(vec![7]), 2, 4, 0),
        ShardRowsError::WrongInput { start_layer: 2 }
    );
    assert_eq!(
        refuse(
            &mut second,
            ShardRowsInput::Hidden(vec![vec![0; d - 1]]),
            2,
            4,
            0
        )
        .kind(),
        "bad_hidden_dim"
    );
    assert_eq!(kv_to_bytes(&second), kv_to_bytes(&KVCache::new(n_layers)));

    let mut promoted = build_model(&fixture, Profile::LegacySplitHalf);
    promoted.enable_i16();
    assert_eq!(
        promoted
            .forward_shard_rows(
                ShardRowsInput::Tokens(vec![1]),
                &mut KVCache::new(n_layers),
                0,
                2,
                0
            )
            .expect_err("an I16-promoted model is refused"),
        ShardRowsError::NotCanonicalProfile
    );
    let mut headless = build_model(&fixture, Profile::LegacySplitHalf);
    headless.output_weight = I8Weights::empty();
    assert_eq!(
        headless
            .forward_shard_rows(
                ShardRowsInput::Tokens(vec![1]),
                &mut KVCache::new(n_layers),
                0,
                n_layers,
                0
            )
            .expect_err("a last stage without the head is refused"),
        ShardRowsError::HeadNotLoaded
    );
    let mut embeddingless = build_model(&fixture, Profile::LegacySplitHalf);
    embeddingless.embedding_q16.clear();
    assert_eq!(
        embeddingless
            .forward_shard_rows(
                ShardRowsInput::Tokens(vec![1]),
                &mut KVCache::new(n_layers),
                0,
                2,
                0
            )
            .expect_err("a first stage without the embedding is refused"),
        ShardRowsError::TokenNotEmbedded { token: 1 }
    );

    assert_eq!(
        model.rollback_rows(&mut holder, 3),
        Err(ShardRowsError::RollbackGrows {
            keep: 3,
            cached_positions: 2
        })
    );
    model
        .rollback_rows(&mut holder, 2)
        .expect("keeping every position is a no-op");
    assert_eq!(
        kv_to_bytes(&holder),
        before,
        "a no-op rollback changed the cache"
    );
    let mut ragged = clone_kv(&holder);
    ragged.k_data[1].push(0);
    let ragged_before = kv_to_bytes(&ragged);
    assert_eq!(
        model
            .rollback_rows(&mut ragged, 1)
            .map_err(|error| error.kind()),
        Err("ragged_cache")
    );
    assert_eq!(
        kv_to_bytes(&ragged),
        ragged_before,
        "a refused rollback changed the cache"
    );

    // A torn stage cache is refused by the multi-row call too: a partial K
    // row, which a floor-divided count would read as whole rows, and a V
    // layer longer than its K layer.
    let d_kv = model.config.d_kv;
    let mut partial = clone_kv(&holder);
    partial.k_data[0].push(0);
    let partial_before = kv_to_bytes(&partial);
    assert_eq!(
        refuse(&mut partial, ShardRowsInput::Tokens(vec![7]), 0, 2, 2),
        ShardRowsError::RaggedCache {
            layer: 0,
            keys: 2 * d_kv + 1,
            values: 2 * d_kv,
            expected: 2 * d_kv
        }
    );
    assert_eq!(
        kv_to_bytes(&partial),
        partial_before,
        "a refused call changed the cache"
    );
    let mut skewed = clone_kv(&holder);
    skewed.v_data[1].extend(std::iter::repeat_n(0, d_kv));
    let skewed_before = kv_to_bytes(&skewed);
    assert_eq!(
        refuse(&mut skewed, ShardRowsInput::Tokens(vec![7]), 0, 2, 2),
        ShardRowsError::RaggedCache {
            layer: 1,
            keys: 2 * d_kv,
            values: 3 * d_kv,
            expected: 2 * d_kv
        }
    );
    assert_eq!(
        kv_to_bytes(&skewed),
        skewed_before,
        "a refused call changed the cache"
    );

    // The rollback refuses a K layer without its V rows, and K and V layer
    // counts that disagree.
    let mut half = clone_kv(&holder);
    half.v_data[0].clear();
    let half_before = kv_to_bytes(&half);
    assert_eq!(
        model.rollback_rows(&mut half, 1),
        Err(ShardRowsError::RaggedCache {
            layer: 0,
            keys: 2 * d_kv,
            values: 0,
            expected: 2 * d_kv
        })
    );
    assert_eq!(
        kv_to_bytes(&half),
        half_before,
        "a refused rollback changed the cache"
    );
    let mut short = clone_kv(&holder);
    short.v_data.pop();
    assert_eq!(
        model.rollback_rows(&mut short, 1),
        Err(ShardRowsError::CacheLayersDisagree {
            k_layers: n_layers,
            v_layers: n_layers - 1
        })
    );
}

/// A multi-row call is held to the prefill's scratch budget: the cap itself is
/// accepted, one more row is refused before anything is touched, and where the
/// budget binds below the 1,024-row ceiling the cap is the budget's.
#[test]
fn golden_modes_stage_rows_respect_the_row_budget() {
    use crate::canonical_prefill::{MAX_PREFILL_SCRATCH_BYTES, prefill_chunk_scratch_bytes};
    use crate::canonical_simd::MAX_BATCH_TOKENS;

    // At the fixture's width the budget is far above the ceiling, so the
    // ceiling binds; give the RoPE table room past it.
    let mut long = fixture();
    long.max_seq = MAX_BATCH_TOKENS + 8;
    let model = build_model(&long, Profile::LegacySplitHalf);
    let n_layers = model.config.n_layers;
    let cap = model.max_shard_rows(1);
    assert_eq!(cap, MAX_BATCH_TOKENS);
    assert_eq!(model.max_shard_rows(n_layers), MAX_BATCH_TOKENS);
    let vocab = u32::try_from(model.config.vocab_size).expect("vocabulary fits u32");
    let tokens: Vec<u32> = (0..cap)
        .map(|index| u32::try_from(index).expect("index fits u32") % vocab)
        .collect();
    let mut holder = KVCache::new(n_layers);
    model
        .forward_shard_rows(ShardRowsInput::Tokens(tokens.clone()), &mut holder, 0, 1, 0)
        .expect("a call at the cap is accepted");
    assert_eq!(holder.seq_len, cap);
    let mut over = tokens;
    over.push(1);
    let mut fresh = KVCache::new(n_layers);
    assert_eq!(
        model.forward_shard_rows(ShardRowsInput::Tokens(over), &mut fresh, 0, 1, 0),
        Err(ShardRowsError::TooManyRows {
            rows: cap + 1,
            max_rows: cap
        })
    );
    assert_eq!(
        kv_to_bytes(&fresh),
        kv_to_bytes(&KVCache::new(n_layers)),
        "a refused call changed the cache"
    );

    // A far wider FFN and vocabulary make the scratch budget bind, harder on
    // the last stage, which holds the logits. The call is refused from the
    // configuration alone, before any weight is read.
    let mut wide = build_model(&fixture(), Profile::LegacySplitHalf);
    wide.config.d_ff = 1 << 15;
    wide.config.vocab_size = 1 << 14;
    let config = wide.config.clone();
    let scratch = prefill_chunk_scratch_bytes(1, config.d_model, config.d_kv, config.d_ff);
    let inner_cap = MAX_PREFILL_SCRATCH_BYTES / (scratch + 8 * config.d_model);
    let last_cap =
        MAX_PREFILL_SCRATCH_BYTES / (scratch + 8 * (config.d_model + 2 * config.vocab_size));
    assert!(last_cap < inner_cap && inner_cap < MAX_BATCH_TOKENS);
    assert_eq!(wide.max_shard_rows(1), inner_cap);
    assert_eq!(wide.max_shard_rows(n_layers), last_cap);
    let mut untouched = KVCache::new(n_layers);
    assert_eq!(
        wide.forward_shard_rows(
            ShardRowsInput::Tokens(vec![1; inner_cap + 1]),
            &mut untouched,
            0,
            1,
            0
        ),
        Err(ShardRowsError::TooManyRows {
            rows: inner_cap + 1,
            max_rows: inner_cap
        })
    );
    assert_eq!(
        kv_to_bytes(&untouched),
        kv_to_bytes(&KVCache::new(n_layers))
    );
}
