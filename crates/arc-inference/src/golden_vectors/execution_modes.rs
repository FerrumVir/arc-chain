use super::{
    GoldenFixture, build_fixture_model, fixture, hash_cache, hash_i64, operators, text, token_ids,
};
use crate::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel,
    GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, I8Weights, KVCache, MultiRowsError, SeqRows,
    ShardInput, ShardOutput, ShardRowsError, ShardRowsInput, ShardRowsOutput,
    select_next_token_with_repetition_penalty,
};
use crate::canonical_simd::{self, BatchedKernel};
use arc_crypto::hash_bytes;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::MutexGuard;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    LegacySplitHalf,
    GgufInterleaved,
}

const PROFILES: [Profile; 2] = [Profile::LegacySplitHalf, Profile::GgufInterleaved];

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kernel {
    Scalar,
    Vectorised(BatchedKernel),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Leg {
    kernel: Kernel,
    threads: usize,
}

const BASE_LEG: Leg = Leg {
    kernel: Kernel::Scalar,
    threads: 1,
};

fn thread_counts() -> [usize; 3] {
    let cores = std::thread::available_parallelism().map_or(4, |cores| cores.get());
    [1, 2, cores.max(4)]
}

struct KernelSwitch {
    _lock: MutexGuard<'static, ()>,
    previous: bool,
    previous_batched: Option<BatchedKernel>,
    pools: BTreeMap<usize, ThreadPool>,
}

impl KernelSwitch {
    fn hold() -> Self {
        let lock = canonical_simd::kernel_switch_guard();
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
        println!("golden_modes legs on this runner: {:?}", switch.legs());
        switch
    }

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
        println!(
            "golden_modes multi-row projections by kernel so far: {}",
            canonical_simd::batched_kernel_run_report()
        );
        canonical_simd::set_fast_canonical_kernel(self.previous);
        canonical_simd::set_batched_kernel_preference(self.previous_batched);
    }
}

#[derive(Default)]
struct Trace {
    logits: BTreeMap<usize, Vec<i64>>,
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

#[derive(Clone, Copy, Debug)]
enum Schedule {
    TokenByToken,
    Chunked(usize),
    LastOnly(usize),
}

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

fn run_fresh(model: &CachedIntegerModel, schedule: Schedule, tokens: &[u32]) -> (Trace, KVCache) {
    let mut kv = empty_kv(model);
    let mut trace = Trace::default();
    feed(model, schedule, tokens, &mut kv, &mut trace);
    (trace, kv)
}

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

const KV_IMAGE_MAGIC: [u8; 8] = *b"ARCKVv01";

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

fn select(logits: &[i64], history: &[u32]) -> u32 {
    let mut logits = logits.to_vec();
    select_next_token_with_repetition_penalty(&mut logits, history)
}

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

#[derive(Clone, Copy, Debug)]
enum Generation {
    Worker,
    V2,
}

const GENERATIONS: [Generation; 2] = [Generation::Worker, Generation::V2];

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

fn recheck(trace: &Trace, first_row: usize, count: usize) -> Vec<u32> {
    let mut tokens = Vec::with_capacity(count);
    for row in first_row..first_row + count {
        let next = select(&trace.logits[&row], &tokens);
        tokens.push(next);
    }
    tokens
}

#[derive(Clone, Copy, Debug)]
enum Drafts {
    AllRight,
    AllWrong,
    LastWrong,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SpeculationCounts {
    accepted_drafts: usize,
    rejected_rounds: usize,
    rolled_back_rows: usize,
}

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
        prefill(model, &[pending], &mut kv, 1, true, &mut trace);
    }
    Speculation {
        tokens,
        trace,
        kv,
        counts,
    }
}

#[derive(Default)]
struct StageTrace {
    boundaries: BTreeMap<(usize, usize), String>,
    terminal: BTreeMap<usize, (u32, String)>,
}

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

fn full_window(fixture: &GoldenFixture) -> Vec<u32> {
    let mut tokens = fixture.sequence_tokens.clone();
    tokens.extend_from_slice(&[2, 19, 22, 0, 17, 4, 9, 21, 6, 15]);
    assert_eq!(tokens.len(), fixture.max_seq);
    tokens
}

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

const REFERENCE_JSON: &str = include_str!("../../tests/fixtures/execution_modes_reference.json");

fn pinned() -> serde_json::Value {
    let document: serde_json::Value =
        serde_json::from_str(REFERENCE_JSON).expect("the pinned reference must be valid JSON");
    assert_eq!(document["schema"], 1, "unsupported pinned reference schema");
    document
}

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

#[test]
#[ignore = "TODO: the engine has no seeded sampler yet; see the doc comment"]
fn golden_modes_seeded_sampling_rechecks_from_teacher_forced_logits() {
    panic!("not implemented: the engine has no seeded sampler to re-check");
}

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

fn generated_history(truth: &[u32], first_row: usize, position: usize) -> Vec<u32> {
    truth[..position.saturating_sub(first_row).min(truth.len())].to_vec()
}

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

struct StageSpeculation {
    tokens: Vec<u32>,
    trace: StageTrace,
    holders: Vec<KVCache>,
    counts: SpeculationCounts,
}

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

#[test]
fn golden_modes_stage_holders_mix_one_row_and_multi_row_calls() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let tokens = full_window(&fixture);
    let len = tokens.len();
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

#[test]
fn golden_modes_stage_rows_respect_the_row_budget() {
    use crate::canonical_prefill::{MAX_PREFILL_SCRATCH_BYTES, prefill_chunk_scratch_bytes};
    use crate::canonical_simd::MAX_BATCH_TOKENS;

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

struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).expect("n fits u64"))
            .expect("a value below n fits usize")
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

fn e4_sequences(fixture: &GoldenFixture) -> Vec<Vec<u32>> {
    let vocab = fixture.vocab_size;
    (1..=fixture.max_seq)
        .map(|len| {
            (0..len)
                .map(|i| u32::try_from((len * 7 + i * 5 + 3) % vocab).expect("token fits u32"))
                .collect()
        })
        .collect()
}

const E4_MAX_BATCH: usize = 64;

struct E4Sequence {
    cache: KVCache,
    logits: BTreeMap<usize, Vec<i64>>,
}

fn e4_call(
    model: &CachedIntegerModel,
    sequences: &[Vec<u32>],
    states: &mut [E4Sequence],
    batch: &[(usize, usize)],
    label: &str,
) {
    let mut taken: Vec<(usize, KVCache)> = batch
        .iter()
        .map(|&(sequence, _)| {
            let cache = std::mem::replace(&mut states[sequence].cache, empty_kv(model));
            (sequence, cache)
        })
        .collect();
    let mut rows: Vec<SeqRows<'_>> = taken
        .iter_mut()
        .zip(batch)
        .map(|((sequence, cache), &(_, count))| {
            let position = cache.seq_len;
            SeqRows {
                cache,
                position,
                input: ShardRowsInput::Tokens(
                    sequences[*sequence][position..position + count].to_vec(),
                ),
            }
        })
        .collect();
    let outputs = model
        .forward_rows_multi(&mut rows, 0, model.config.n_layers)
        .unwrap_or_else(|error| panic!("{label}: batch {batch:?} refused: {error}"));
    drop(rows);
    assert_eq!(
        outputs.len(),
        batch.len(),
        "{label}: one output per sequence"
    );
    for ((sequence, cache), output) in taken.into_iter().zip(outputs) {
        let ShardRowsOutput::Logits(logit_rows) = output else {
            panic!("{label}: a whole-model call returned hidden rows");
        };
        let first = cache.seq_len - logit_rows.len();
        for (offset, logits) in logit_rows.into_iter().enumerate() {
            let previous = states[sequence].logits.insert(first + offset, logits);
            assert!(
                previous.is_none(),
                "{label}: sequence {sequence} returned position {} twice",
                first + offset
            );
        }
        states[sequence].cache = cache;
    }
}

fn e4_assert(references: &[Reference], states: &[E4Sequence], label: &str) {
    for (sequence, (reference, state)) in references.iter().zip(states).enumerate() {
        let positions = state.cache.seq_len;
        let mode = format!("{label}, sequence {sequence}");
        assert_eq!(state.logits.len(), positions, "{mode}: logit rows");
        for (position, logits) in &state.logits {
            let expected = &reference.trace.logits[position];
            assert!(
                logits == expected,
                "{mode} DIFFERS from the sequence run alone: logits at position {position} \
                 (digest {}, expected {})",
                hash_i64(logits),
                hash_i64(expected)
            );
        }
        reference.assert_kv(&state.cache, positions, &mode);
    }
}

fn e4_left(sequences: &[Vec<u32>], states: &[E4Sequence], s: usize) -> usize {
    sequences[s].len() - states[s].cache.seq_len
}

fn e4_draw_batch(draw: &mut Draw, left: &[usize], wanted: usize) -> Vec<(usize, usize)> {
    let mut open: Vec<usize> = (0..left.len()).filter(|&s| left[s] > 0).collect();
    draw.shuffle(&mut open);
    let mut members = 0usize;
    let mut capacity = 0usize;
    while members < open.len() && members < wanted && capacity < wanted {
        capacity += left[open[members]];
        members += 1;
    }
    let extra = draw.below(open.len().min(wanted) - members + 1);
    for &s in &open[members..members + extra] {
        capacity += left[s];
    }
    members += extra;
    let total = wanted.min(capacity);
    let mut counts = vec![1usize; members];
    let mut spare = total - members;
    while spare > 0 {
        let i = draw.below(members);
        if counts[i] < left[open[i]] {
            counts[i] += 1;
            spare -= 1;
        }
    }
    open[..members].iter().copied().zip(counts).collect()
}

fn e4_sweep(
    model: &CachedIntegerModel,
    sequences: &[Vec<u32>],
    references: &[Reference],
    seed: u64,
    sizes: &[usize],
    label: &str,
) -> usize {
    let mut draw = Draw(seed);
    let mut next_size = 0usize;
    let mut schedules = 0usize;
    while next_size < sizes.len() {
        schedules += 1;
        let mut states: Vec<E4Sequence> = sequences
            .iter()
            .map(|_| E4Sequence {
                cache: empty_kv(model),
                logits: BTreeMap::new(),
            })
            .collect();
        loop {
            let left: Vec<usize> = (0..sequences.len())
                .map(|s| e4_left(sequences, &states, s))
                .collect();
            if left.iter().all(|&rows| rows == 0) {
                break;
            }
            let wanted = sizes[next_size.min(sizes.len() - 1)];
            let batch = e4_draw_batch(&mut draw, &left, wanted);
            let total: usize = batch.iter().map(|&(_, rows)| rows).sum();
            e4_call(model, sequences, &mut states, &batch, label);
            if total == wanted && next_size < sizes.len() {
                next_size += 1;
            }
        }
        e4_assert(
            references,
            &states,
            &format!("{label}, schedule {schedules}"),
        );
    }
    schedules
}

#[test]
fn golden_modes_multi_sequence_batches_match_each_sequence_alone() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let sequences = e4_sequences(&fixture);
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let references: Vec<Reference> = switch.run(BASE_LEG, || {
            sequences
                .iter()
                .map(|tokens| Reference::new(&model, tokens))
                .collect()
        });
        for (index, leg) in switch.legs().into_iter().enumerate() {
            let seed =
                0x9E37_79B9_7F4A_7C15 ^ (u64::try_from(index).expect("leg index fits u64") << 32);
            let mut sizes: Vec<usize> = (1..=E4_MAX_BATCH).collect();
            Draw(seed ^ 0xA5A5).shuffle(&mut sizes);
            let label = format!("E4 {profile:?} {leg:?}");
            let schedules = switch.run(leg, || {
                e4_sweep(&model, &sequences, &references, seed, &sizes, &label)
            });
            if leg == BASE_LEG {
                println!(
                    "{label}: every batch size 1..={E4_MAX_BATCH} ran, over {schedules} schedules"
                );
            }
        }
    }
}

#[test]
fn golden_modes_multi_sequence_interrupted_sequence_resumes_with_new_neighbours() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let sequences = e4_sequences(&fixture);
    let resumed = sequences.len() - 1;
    let (b, c, d, e) = (3, 8, 5, 12);
    for profile in PROFILES {
        let first = build_model(&fixture, profile);
        let second = build_model(&fixture, profile);
        let references: Vec<Reference> = switch.run(BASE_LEG, || {
            sequences
                .iter()
                .map(|tokens| Reference::new(&first, tokens))
                .collect()
        });
        for leg in switch.legs() {
            let label = format!("E4 interruption {profile:?} {leg:?}");
            switch.run(leg, || {
                let mut states: Vec<E4Sequence> = sequences
                    .iter()
                    .map(|_| E4Sequence {
                        cache: empty_kv(&first),
                        logits: BTreeMap::new(),
                    })
                    .collect();
                e4_call(
                    &first,
                    &sequences,
                    &mut states,
                    &[(c, 5), (resumed, 6), (b, 4)],
                    &label,
                );
                let image = kv_to_bytes(&states[resumed].cache);
                states[resumed].cache = kv_from_bytes(&image);
                e4_call(
                    &second,
                    &sequences,
                    &mut states,
                    &[(d, 6), (resumed, 6), (e, 7)],
                    &label,
                );
                e4_call(
                    &second,
                    &sequences,
                    &mut states,
                    &[(e, 6), (resumed, 3)],
                    &label,
                );
                e4_call(&second, &sequences, &mut states, &[(resumed, 1)], &label);
                assert_eq!(states[resumed].cache.seq_len, sequences[resumed].len());
                for s in [resumed, b, c, d, e] {
                    e4_assert(
                        std::slice::from_ref(&references[s]),
                        std::slice::from_ref(&states[s]),
                        &format!("{label}, sequence {s}"),
                    );
                }
            });
        }
    }
}

#[test]
fn golden_modes_multi_sequence_refusal_changes_no_cache() {
    use crate::canonical_simd::MAX_BATCH_TOKENS;

    fn image(caches: &[KVCache]) -> Vec<Vec<u8>> {
        caches.iter().map(kv_to_bytes).collect()
    }

    let model = build_model(&fixture(), Profile::LegacySplitHalf);
    let n_layers = model.config.n_layers;

    let mut good = empty_kv(&model);
    feed(
        &model,
        Schedule::TokenByToken,
        &[3, 4],
        &mut good,
        &mut Trace::default(),
    );
    let mut caches = [clone_kv(&good), empty_kv(&model), clone_kv(&good)];
    let before = image(&caches);
    {
        let [first, second, third] = &mut caches[..] else {
            unreachable!("three caches")
        };
        let mut batch = [
            SeqRows {
                cache: first,
                position: 2,
                input: ShardRowsInput::Tokens(vec![5, 6]),
            },
            SeqRows {
                cache: second,
                position: 0,
                input: ShardRowsInput::Tokens(vec![7]),
            },
            SeqRows {
                cache: third,
                position: 1,
                input: ShardRowsInput::Tokens(vec![8]),
            },
        ];
        let refused = model.forward_rows_multi(&mut batch, 0, n_layers);
        assert!(
            matches!(
                refused,
                Err(MultiRowsError::Sequence {
                    index: 2,
                    error: ShardRowsError::Shard(_)
                })
            ),
            "{refused:?}"
        );
    }
    assert_eq!(image(&caches), before, "a refused batch changed a cache");

    {
        let [first, second, _] = &mut caches[..] else {
            unreachable!("three caches")
        };
        let mut batch = [
            SeqRows {
                cache: first,
                position: 2,
                input: ShardRowsInput::Tokens(vec![5]),
            },
            SeqRows {
                cache: second,
                position: 0,
                input: ShardRowsInput::Hidden(vec![vec![0; model.config.d_model]]),
            },
        ];
        assert_eq!(
            model.forward_rows_multi(&mut batch, 0, n_layers),
            Err(MultiRowsError::Sequence {
                index: 1,
                error: ShardRowsError::WrongInput { start_layer: 0 }
            })
        );
    }
    assert_eq!(image(&caches), before, "a refused batch changed a cache");

    assert_eq!(
        model.forward_rows_multi(&mut [], 0, n_layers),
        Err(MultiRowsError::Batch(ShardRowsError::NoRows))
    );

    let per_sequence = model.config.max_seq;
    let count = MAX_BATCH_TOKENS / per_sequence + 1;
    let mut many: Vec<KVCache> = (0..count).map(|_| empty_kv(&model)).collect();
    let many_before = image(&many);
    let mut batch: Vec<SeqRows<'_>> = many
        .iter_mut()
        .map(|cache| SeqRows {
            cache,
            position: 0,
            input: ShardRowsInput::Tokens(vec![1; per_sequence]),
        })
        .collect();
    assert_eq!(
        model.forward_rows_multi(&mut batch, 0, n_layers),
        Err(MultiRowsError::Batch(ShardRowsError::TooManyRows {
            rows: count * per_sequence,
            max_rows: model.max_shard_rows(n_layers)
        }))
    );
    drop(batch);
    assert_eq!(image(&many), many_before, "a refused batch changed a cache");
}

struct E4StageSequence {
    holders: Vec<KVCache>,
    logits: BTreeMap<usize, Vec<i64>>,
    boundaries: BTreeMap<(usize, usize), String>,
}

fn e4_stage_call(
    model: &CachedIntegerModel,
    ends: &[usize],
    sequences: &[Vec<u32>],
    states: &mut [E4StageSequence],
    batch: &[(usize, usize)],
    label: &str,
) {
    let n_layers = model.config.n_layers;
    let positions: Vec<usize> = batch
        .iter()
        .map(|&(sequence, _)| states[sequence].holders[0].seq_len)
        .collect();
    let mut carried: Option<Vec<Vec<Vec<i64>>>> = None;
    let mut start = 0;
    for (stage, &end) in ends.iter().enumerate() {
        let mut taken: Vec<KVCache> = batch
            .iter()
            .map(|&(sequence, _)| {
                std::mem::replace(&mut states[sequence].holders[stage], empty_kv(model))
            })
            .collect();
        let inputs: Vec<ShardRowsInput> = match carried.take() {
            None => batch
                .iter()
                .zip(&positions)
                .map(|(&(sequence, count), &position)| {
                    ShardRowsInput::Tokens(sequences[sequence][position..position + count].to_vec())
                })
                .collect(),
            Some(hidden) => hidden.into_iter().map(ShardRowsInput::Hidden).collect(),
        };
        let mut rows: Vec<SeqRows<'_>> = taken
            .iter_mut()
            .zip(&positions)
            .zip(inputs)
            .map(|((cache, &position), input)| SeqRows {
                cache,
                position,
                input,
            })
            .collect();
        let outputs = model
            .forward_rows_multi(&mut rows, start, end)
            .unwrap_or_else(|error| {
                panic!("{label}: stage [{start}, {end}) refused batch {batch:?}: {error}")
            });
        drop(rows);
        for (&(sequence, _), cache) in batch.iter().zip(taken) {
            states[sequence].holders[stage] = cache;
        }
        if end == n_layers {
            for ((&(sequence, _), &position), output) in batch.iter().zip(&positions).zip(outputs) {
                let ShardRowsOutput::Logits(logit_rows) = output else {
                    panic!("{label}: the last stage returned hidden rows");
                };
                for (offset, logits) in logit_rows.into_iter().enumerate() {
                    let previous = states[sequence].logits.insert(position + offset, logits);
                    assert!(previous.is_none(), "{label}: logits returned twice");
                }
            }
            return;
        }
        let mut hidden = Vec::with_capacity(batch.len());
        for ((&(sequence, _), &position), output) in batch.iter().zip(&positions).zip(outputs) {
            let ShardRowsOutput::Hidden(stage_rows) = output else {
                panic!("{label}: stage [{start}, {end}) returned logits");
            };
            for (offset, row) in stage_rows.iter().enumerate() {
                let previous = states[sequence]
                    .boundaries
                    .insert((position + offset, end - 1), hash_i64(row));
                assert!(previous.is_none(), "{label}: a boundary was reported twice");
            }
            hidden.push(stage_rows);
        }
        carried = Some(hidden);
        start = end;
    }
    panic!("{label}: the last stage of {ends:?} returned hidden rows");
}

fn e4_stage_run(
    model: &CachedIntegerModel,
    ends: &[usize],
    sequences: &[Vec<u32>],
    references: &[Reference],
    seed: u64,
    label: &str,
) {
    let mut draw = Draw(seed);
    let mut states: Vec<E4StageSequence> = sequences
        .iter()
        .map(|_| E4StageSequence {
            holders: stage_holders(model, ends),
            logits: BTreeMap::new(),
            boundaries: BTreeMap::new(),
        })
        .collect();
    loop {
        let left: Vec<usize> = sequences
            .iter()
            .zip(&states)
            .map(|(tokens, state)| tokens.len() - state.holders[0].seq_len)
            .collect();
        if left.iter().all(|&rows| rows == 0) {
            break;
        }
        let wanted = 1 + draw.below(E4_MAX_BATCH / 2);
        let batch = e4_draw_batch(&mut draw, &left, wanted);
        e4_stage_call(model, ends, sequences, &mut states, &batch, label);
    }
    let cuts = &ends[..ends.len() - 1];
    for (sequence, (reference, state)) in references.iter().zip(&states).enumerate() {
        let positions = state.holders[0].seq_len;
        let mode = format!("{label}, sequence {sequence}");
        assert_eq!(state.logits.len(), positions, "{mode}: logit rows");
        for (position, logits) in &state.logits {
            assert!(
                logits == &reference.trace.logits[position],
                "{mode} DIFFERS from the sequence run alone: logits at position {position}"
            );
        }
        for position in 0..positions {
            for &end in cuts {
                let layer = end - 1;
                let actual = state.boundaries.get(&(position, layer)).unwrap_or_else(|| {
                    panic!("{mode}: no boundary at position {position}, layer {layer}")
                });
                assert!(
                    actual == &reference.trace.boundaries[&(position, layer)],
                    "{mode} DIFFERS from the sequence run alone: the hidden row leaving \
                     layer {layer} at position {position}"
                );
            }
        }
        reference.assert_kv(&merge_holders(ends, &state.holders), positions, &mode);
    }
}

#[test]
fn golden_modes_multi_sequence_stage_splits_match_each_sequence_alone() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let sequences = e4_sequences(&fixture);
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let references: Vec<Reference> = switch.run(BASE_LEG, || {
            sequences
                .iter()
                .map(|tokens| Reference::new(&model, tokens))
                .collect()
        });
        let splits: Vec<Vec<usize>> = stage_splits(model.config.n_layers)
            .into_iter()
            .filter(|ends| ends.len() == 2 || ends.len() == 4)
            .collect();
        for (index, leg) in switch.legs().into_iter().enumerate() {
            for ends in &splits {
                let seed = 0xD1B5_4A32_D192_ED03
                    ^ (u64::try_from(index).expect("leg index fits u64") << 32)
                    ^ u64::try_from(ends[0] * 8 + ends.len()).expect("split fits u64");
                let label = format!("E4 stages {ends:?} {profile:?} {leg:?}");
                switch.run(leg, || {
                    e4_stage_run(&model, ends, &sequences, &references, seed, &label)
                });
            }
        }
    }
}

fn generate_together(
    model: &CachedIntegerModel,
    prompts: &[Vec<u32>],
    budgets: &[usize],
) -> Vec<(Vec<u32>, String)> {
    let n_layers = model.config.n_layers;
    let bos = model.config.bos_token;
    let mut caches: Vec<KVCache> = prompts.iter().map(|_| empty_kv(model)).collect();
    {
        let mut batch: Vec<SeqRows<'_>> = caches
            .iter_mut()
            .zip(prompts)
            .map(|(cache, prompt)| {
                let mut tokens = vec![bos];
                tokens.extend_from_slice(prompt);
                SeqRows {
                    cache,
                    position: 0,
                    input: ShardRowsInput::Tokens(tokens),
                }
            })
            .collect();
        model
            .forward_rows_multi(&mut batch, 0, n_layers)
            .expect("the prompts are accepted");
    }
    let mut generated: Vec<Vec<u32>> = prompts.iter().map(|_| Vec::new()).collect();
    loop {
        let active: Vec<usize> = (0..prompts.len())
            .filter(|&s| generated[s].len() < budgets[s])
            .collect();
        if active.is_empty() {
            break;
        }
        let mut taken: Vec<KVCache> = active
            .iter()
            .map(|&s| std::mem::replace(&mut caches[s], empty_kv(model)))
            .collect();
        let mut batch: Vec<SeqRows<'_>> = taken
            .iter_mut()
            .zip(&active)
            .map(|(cache, &s)| {
                let last = generated[s]
                    .last()
                    .copied()
                    .unwrap_or_else(|| *prompts[s].last().expect("a non-empty prompt"));
                let position = cache.seq_len;
                SeqRows {
                    cache,
                    position,
                    input: ShardRowsInput::Tokens(vec![last]),
                }
            })
            .collect();
        let outputs = model
            .forward_rows_multi(&mut batch, 0, n_layers)
            .expect("a decode step is accepted");
        drop(batch);
        for ((&s, cache), output) in active.iter().zip(taken).zip(outputs) {
            let ShardRowsOutput::Logits(mut rows) = output else {
                panic!("a whole-model call returned hidden rows");
            };
            let logits = rows.pop().expect("one logits row per sequence");
            let next = select(&logits, &generated[s]);
            generated[s].push(next);
            caches[s] = cache;
        }
    }
    generated
        .into_iter()
        .map(|tokens| {
            let hash = output_hash(&tokens);
            (tokens, hash)
        })
        .collect()
}

#[test]
fn golden_modes_multi_sequence_decode_selects_like_try_generate() {
    let switch = KernelSwitch::hold();
    let fixture = fixture();
    let vocab = fixture.vocab_size;
    let prompts: Vec<Vec<u32>> = (2..=6usize)
        .map(|len| {
            (0..len)
                .map(|i| u32::try_from((len * 11 + i * 3 + 1) % vocab).expect("token fits u32"))
                .collect()
        })
        .collect();
    let budgets: Vec<usize> = prompts
        .iter()
        .map(|prompt| fixture.max_seq - 1 - prompt.len() - prompt.len() % 3)
        .collect();
    for profile in PROFILES {
        let model = build_model(&fixture, profile);
        let expected: Vec<(Vec<u32>, String)> = switch.run(BASE_LEG, || {
            prompts
                .iter()
                .zip(&budgets)
                .map(|(prompt, &budget)| {
                    production_generate(
                        &model,
                        Generation::Worker,
                        prompt,
                        u32::try_from(budget).expect("budget fits u32"),
                    )
                })
                .collect()
        });
        for leg in switch.legs() {
            let got = switch.run(leg, || generate_together(&model, &prompts, &budgets));
            for (sequence, (got, want)) in got.iter().zip(&expected).enumerate() {
                assert_eq!(
                    got, want,
                    "{profile:?} {leg:?}: sequence {sequence} decoded together differs from \
                     its own try_generate"
                );
            }
        }
    }
}
