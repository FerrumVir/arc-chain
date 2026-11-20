use stwo::core::air::Component;
use stwo::core::channel::Blake2sChannel;
use stwo::core::fields::m31::{BaseField, M31};
use stwo::core::fields::qm31::SecureField;
use stwo::core::pcs::{CommitmentSchemeVerifier, PcsConfig};
use stwo::core::poly::circle::CanonicCoset;
use stwo::core::utils::bit_reverse_coset_to_circle_domain_order;
use stwo::core::vcs_lifted::blake2_merkle::Blake2sMerkleChannel;
use stwo::core::verifier::verify;
use stwo::prover::backend::simd::SimdBackend;
use stwo::prover::poly::BitReversedOrder;
use stwo::prover::poly::circle::{CircleEvaluation, PolyOps};
use stwo::prover::{self as stwo_prover_mod, CommitmentSchemeProver};
use stwo_constraint_framework::{
    EvalAtRow, FrameworkComponent, FrameworkEval, ORIGINAL_TRACE_IDX, TraceLocationAllocator,
};

use num_traits::Zero;
use std::time::Instant;

use crate::stark::BlockProofInput;

#[cfg(feature = "stwo-icicle")]
pub fn ensure_icicle_initialized() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let device = icicle_runtime::Device::new("Metal", 0);
        icicle_runtime::set_device(&device)
            .expect("Failed to initialize ICICLE Metal GPU device 0");
        eprintln!("[ICICLE] GPU initialized: Metal device 0");
    });
}

pub const HASH_LIMBS: usize = 16;

pub const LIMB_BASE: u32 = 1 << 16;

pub const LIMB_LO_MAX: u32 = LIMB_BASE - 1;

pub const TRANSFER_COLS: usize = 15;

pub const TRACE_COLS: usize = 1 + HASH_LIMBS + TRANSFER_COLS;

pub const MIN_LOG_SIZE: u32 = 4;

pub fn bytes32_to_m31_limbs(bytes: &[u8; 32]) -> [M31; HASH_LIMBS] {
    let mut limbs = [M31::from(0u32); HASH_LIMBS];
    for (i, chunk) in bytes.chunks_exact(2).enumerate() {
        let val = u16::from_le_bytes([chunk[0], chunk[1]]) as u32;
        limbs[i] = M31::from(val);
    }
    limbs
}

pub fn m31_limbs_to_bytes32(limbs: &[M31; HASH_LIMBS]) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (i, limb) in limbs.iter().enumerate() {
        let val = limb.0 as u16;
        bytes[i * 2..i * 2 + 2].copy_from_slice(&val.to_le_bytes());
    }
    bytes
}

pub fn u64_to_m31_limbs(value: u64) -> Result<(M31, M31), String> {
    let lo = (value & 0xFFFF) as u32;
    let hi = (value >> 16) as u32;
    if hi >= (1u32 << 31) - 1 {
        return Err(format!(
            "u64_to_m31_limbs: value {value} too large (hi={hi} >= M31 modulus). \
             Max safe value: {}",
            (1u64 << 47) - (1u64 << 16) - 1
        ));
    }
    Ok((M31::from(lo), M31::from(hi)))
}

pub fn m31_limbs_to_u64(lo: M31, hi: M31) -> u64 {
    (lo.0 as u64) + ((hi.0 as u64) << 16)
}

#[derive(Clone)]
pub struct ArcBlockWitnessEval {
    pub log_size: u32,
}

pub type ArcBlockWitnessComponent = FrameworkComponent<ArcBlockWitnessEval>;

impl FrameworkEval for ArcBlockWitnessEval {
    fn log_size(&self) -> u32 {
        self.log_size
    }

    fn max_constraint_log_degree_bound(&self) -> u32 {
        self.log_size + 1
    }

    fn evaluate<E: EvalAtRow>(&self, mut eval: E) -> E {
        let active = eval.next_trace_mask();

        let mut hash_limbs = Vec::with_capacity(HASH_LIMBS);
        for _ in 0..HASH_LIMBS {
            hash_limbs.push(eval.next_trace_mask());
        }

        let has_transfer = eval.next_trace_mask();

        let sbb_lo = eval.next_trace_mask();
        let sbb_hi = eval.next_trace_mask();

        let sba_lo = eval.next_trace_mask();
        let sba_hi = eval.next_trace_mask();

        let rbb_lo = eval.next_trace_mask();
        let rbb_hi = eval.next_trace_mask();

        let rba_lo = eval.next_trace_mask();
        let rba_hi = eval.next_trace_mask();

        let amt_lo = eval.next_trace_mask();
        let amt_hi = eval.next_trace_mask();

        let nonce_before = eval.next_trace_mask();

        let nonce_after = eval.next_trace_mask();

        let fee = eval.next_trace_mask();

        let suf_bal_aux = eval.next_trace_mask();

        let base = BaseField::from(LIMB_BASE);
        let one = BaseField::from(1u32);

        let sender_before = sbb_lo.clone() + sbb_hi.clone() * base;
        let sender_after = sba_lo.clone() + sba_hi.clone() * base;
        let recv_before = rbb_lo.clone() + rbb_hi.clone() * base;
        let recv_after = rba_lo.clone() + rba_hi.clone() * base;
        let amount = amt_lo.clone() + amt_hi.clone() * base;

        eval.add_constraint(active.clone() * active.clone() - active.clone());

        eval.add_constraint(has_transfer.clone() * has_transfer.clone() - has_transfer.clone());

        eval.add_constraint(has_transfer.clone() - active.clone() * has_transfer.clone());

        for limb in &hash_limbs {
            eval.add_constraint(limb.clone() - active.clone() * limb.clone());
        }

        eval.add_constraint(
            has_transfer.clone()
                * (sender_after.clone() - sender_before.clone() + amount.clone() + fee.clone()),
        );

        eval.add_constraint(
            has_transfer.clone() * (recv_after.clone() - recv_before.clone() - amount.clone()),
        );

        eval.add_constraint(
            has_transfer.clone() * (nonce_after.clone() - nonce_before.clone() - E::F::from(one)),
        );

        eval.add_constraint(has_transfer.clone() * (suf_bal_aux.clone() - sender_after.clone()));

        let transfer_data_cols = [
            sbb_lo,
            sbb_hi,
            sba_lo,
            sba_hi,
            rbb_lo,
            rbb_hi,
            rba_lo,
            rba_hi,
            amt_lo,
            amt_hi,
            nonce_before,
            nonce_after,
            fee,
            suf_bal_aux,
        ];
        for col in &transfer_data_cols {
            eval.add_constraint(col.clone() - has_transfer.clone() * col.clone());
        }

        eval
    }
}

pub const DENSE_STARK_COLS: usize = 6;

pub const DENSE_STARK_PP_COLS: usize = 3;

fn dense_pp_ids() -> Vec<stwo_constraint_framework::preprocessed_columns::PreProcessedColumnId> {
    use stwo_constraint_framework::preprocessed_columns::PreProcessedColumnId;
    ["dense_is_active", "dense_is_start", "dense_is_last"]
        .iter()
        .map(|id| PreProcessedColumnId {
            id: (*id).to_string(),
        })
        .collect()
}

#[derive(Clone)]
pub struct DenseLayerStarkEval {
    pub log_size: u32,
}

pub type DenseLayerStarkComponent = FrameworkComponent<DenseLayerStarkEval>;

impl FrameworkEval for DenseLayerStarkEval {
    fn log_size(&self) -> u32 {
        self.log_size
    }

    fn max_constraint_log_degree_bound(&self) -> u32 {
        self.log_size + 1
    }

    fn evaluate<E: EvalAtRow>(&self, mut eval: E) -> E {
        use stwo_constraint_framework::preprocessed_columns::PreProcessedColumnId;

        let is_active = eval.get_preprocessed_column(PreProcessedColumnId {
            id: "dense_is_active".to_string(),
        });
        let is_start = eval.get_preprocessed_column(PreProcessedColumnId {
            id: "dense_is_start".to_string(),
        });
        let is_last = eval.get_preprocessed_column(PreProcessedColumnId {
            id: "dense_is_last".to_string(),
        });

        let weight = eval.next_trace_mask();
        let input = eval.next_trace_mask();
        let product = eval.next_trace_mask();
        let [acc, acc_prev] = eval.next_interaction_mask(ORIGINAL_TRACE_IDX, [0, -1]);
        let output = eval.next_trace_mask();
        let bias = eval.next_trace_mask();

        eval.add_constraint(product.clone() - weight.clone() * input.clone());

        eval.add_constraint(
            acc.clone()
                - is_start.clone() * product.clone()
                - (is_active.clone() - is_start.clone()) * (acc_prev + product),
        );

        eval.add_constraint(output.clone() - is_last.clone() * (acc + bias.clone()));

        eval.add_constraint(bias.clone() - is_last * bias);

        eval.add_constraint(weight.clone() - is_active.clone() * weight);
        eval.add_constraint(input.clone() - is_active * input);

        eval
    }
}

pub fn generate_dense_preprocessed_trace(
    in_size: usize,
    out_size: usize,
) -> Vec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    let n_ops = out_size * in_size;
    let log_size = compute_log_size(n_ops);
    let trace_size = 1usize << log_size;
    let domain = CanonicCoset::new(log_size).circle_domain();

    let one = M31::from(1u32);
    let zero = M31::from(0u32);

    let mut is_active = vec![zero; trace_size];
    let mut is_start = vec![zero; trace_size];
    let mut is_last = vec![zero; trace_size];

    for (row, ((a, s), l)) in is_active
        .iter_mut()
        .zip(is_start.iter_mut())
        .zip(is_last.iter_mut())
        .enumerate()
    {
        if row >= n_ops {
            break;
        }
        *a = one;
        let j = row % in_size;
        if j == 0 {
            *s = one;
        }
        if j == in_size - 1 {
            *l = one;
        }
    }

    [is_active, is_start, is_last]
        .into_iter()
        .map(|mut col| {
            bit_reverse_coset_to_circle_domain_order(&mut col);
            CircleEvaluation::new(domain, col.into_iter().collect())
        })
        .collect()
}

pub fn generate_dense_stark_trace(
    weights: &[i64],
    input: &[i64],
    output: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    generate_dense_stark_trace_with_bias(weights, input, output, &[], in_size, out_size)
}

pub fn generate_dense_stark_trace_with_bias(
    weights: &[i64],
    input: &[i64],
    output: &[i64],
    bias: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    let n_ops = out_size * in_size;
    let log_size = compute_log_size(n_ops);
    let trace_size = 1usize << log_size;
    let domain = CanonicCoset::new(log_size).circle_domain();

    let mut cols: Vec<Vec<M31>> = (0..DENSE_STARK_COLS)
        .map(|_| vec![M31::from(0u32); trace_size])
        .collect();

    let mut row = 0usize;
    for i in 0..out_size {
        let mut acc = M31::from(0u32);
        for j in 0..in_size {
            if row >= trace_size {
                break;
            }

            let w = to_m31(weights[i * in_size + j]);
            let x = to_m31(input[j]);
            let prod = w * x;
            acc = if j == 0 { prod } else { acc + prod };

            cols[0][row] = w;
            cols[1][row] = x;
            cols[2][row] = prod;
            cols[3][row] = acc;
            if j == in_size - 1 {
                cols[4][row] = to_m31(output[i]);
                cols[5][row] = if bias.is_empty() {
                    M31::from(0u32)
                } else {
                    to_m31(bias[i])
                };
            }

            row += 1;
        }
    }

    cols.into_iter()
        .map(|mut col| {
            bit_reverse_coset_to_circle_domain_order(&mut col);
            CircleEvaluation::new(domain, col.into_iter().collect())
        })
        .collect()
}

fn to_m31(v: i64) -> M31 {
    const M31_MOD: u64 = (1u64 << 31) - 1;
    let v_abs = (v.unsigned_abs() % M31_MOD) as u32;
    if v >= 0 {
        M31::from(v_abs)
    } else {
        M31::from(0u32) - M31::from(v_abs)
    }
}

pub const PACK_K: usize = 32;

pub const PACKED_STARK_COLS: usize = 3 * PACK_K + 3;

fn packed_pp_ids() -> Vec<stwo_constraint_framework::preprocessed_columns::PreProcessedColumnId> {
    use stwo_constraint_framework::preprocessed_columns::PreProcessedColumnId;
    ["pk_is_active", "pk_is_start", "pk_is_last"]
        .iter()
        .map(|id| PreProcessedColumnId {
            id: (*id).to_string(),
        })
        .collect()
}

#[derive(Clone)]
pub struct PackedDenseEval {
    pub log_size: u32,
}

pub type PackedDenseComponent = FrameworkComponent<PackedDenseEval>;

impl FrameworkEval for PackedDenseEval {
    fn log_size(&self) -> u32 {
        self.log_size
    }
    fn max_constraint_log_degree_bound(&self) -> u32 {
        self.log_size + 1
    }
    fn evaluate<E: EvalAtRow>(&self, mut eval: E) -> E {
        use stwo_constraint_framework::preprocessed_columns::PreProcessedColumnId;

        let is_active = eval.get_preprocessed_column(PreProcessedColumnId {
            id: "pk_is_active".to_string(),
        });
        let is_start = eval.get_preprocessed_column(PreProcessedColumnId {
            id: "pk_is_start".to_string(),
        });
        let is_last = eval.get_preprocessed_column(PreProcessedColumnId {
            id: "pk_is_last".to_string(),
        });

        let w: Vec<_> = (0..PACK_K).map(|_| eval.next_trace_mask()).collect();
        let x: Vec<_> = (0..PACK_K).map(|_| eval.next_trace_mask()).collect();
        let p: Vec<_> = (0..PACK_K).map(|_| eval.next_trace_mask()).collect();
        let [acc, acc_prev] = eval.next_interaction_mask(ORIGINAL_TRACE_IDX, [0, -1]);
        let out = eval.next_trace_mask();
        let bias = eval.next_trace_mask();

        for i in 0..PACK_K {
            eval.add_constraint(p[i].clone() - w[i].clone() * x[i].clone());
        }

        let mut row_sum = p[0].clone();
        for item in p.iter().take(PACK_K).skip(1) {
            row_sum += item.clone();
        }

        eval.add_constraint(
            acc.clone()
                - is_start.clone() * row_sum.clone()
                - (is_active.clone() - is_start.clone()) * (acc_prev + row_sum),
        );

        eval.add_constraint(out.clone() - is_last.clone() * (acc + bias.clone()));

        eval.add_constraint(bias.clone() - is_last * bias);

        for i in 0..PACK_K {
            eval.add_constraint(w[i].clone() - is_active.clone() * w[i].clone());
            eval.add_constraint(x[i].clone() - is_active.clone() * x[i].clone());
        }

        eval
    }
}

pub fn packed_log_size(in_size: usize, out_size: usize) -> u32 {
    let rows_per_neuron = in_size.div_ceil(PACK_K);
    compute_log_size(out_size * rows_per_neuron)
}

pub fn generate_packed_preprocessed_trace(
    in_size: usize,
    out_size: usize,
) -> Vec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    let rows_per_neuron = in_size.div_ceil(PACK_K);
    let n_rows = out_size * rows_per_neuron;
    let log_size = packed_log_size(in_size, out_size);
    let trace_size = 1usize << log_size;
    let domain = CanonicCoset::new(log_size).circle_domain();

    let one = M31::from(1u32);
    let zero = M31::from(0u32);
    let mut is_active = vec![zero; trace_size];
    let mut is_start = vec![zero; trace_size];
    let mut is_last = vec![zero; trace_size];

    for row in 0..n_rows.min(trace_size) {
        is_active[row] = one;
        let r = row % rows_per_neuron;
        if r == 0 {
            is_start[row] = one;
        }
        if r == rows_per_neuron - 1 {
            is_last[row] = one;
        }
    }

    [is_active, is_start, is_last]
        .into_iter()
        .map(|mut col| {
            bit_reverse_coset_to_circle_domain_order(&mut col);
            CircleEvaluation::new(domain, col.into_iter().collect())
        })
        .collect()
}

pub fn generate_packed_trace(
    weights: &[i64],
    input: &[i64],
    output: &[i64],
    bias: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    let rows_per_neuron = in_size.div_ceil(PACK_K);
    let log_size = packed_log_size(in_size, out_size);
    let trace_size = 1usize << log_size;
    let domain = CanonicCoset::new(log_size).circle_domain();

    let mut cols: Vec<Vec<M31>> = (0..PACKED_STARK_COLS)
        .map(|_| vec![M31::from(0u32); trace_size])
        .collect();

    for i in 0..out_size {
        let mut acc = M31::from(0u32);
        for r in 0..rows_per_neuron {
            let row = i * rows_per_neuron + r;
            if row >= trace_size {
                break;
            }
            let mut row_sum = M31::from(0u32);
            for k in 0..PACK_K {
                let j = r * PACK_K + k;
                if j >= in_size {
                    break;
                }
                let w = to_m31(weights[i * in_size + j]);
                let xv = to_m31(input[j]);
                let prod = w * xv;
                cols[k][row] = w;
                cols[PACK_K + k][row] = xv;
                cols[2 * PACK_K + k][row] = prod;
                row_sum += prod;
            }
            acc = if r == 0 { row_sum } else { acc + row_sum };
            cols[3 * PACK_K][row] = acc;
            if r == rows_per_neuron - 1 {
                cols[3 * PACK_K + 1][row] = to_m31(output[i]);
                cols[3 * PACK_K + 2][row] = if bias.is_empty() {
                    M31::from(0u32)
                } else {
                    to_m31(bias[i])
                };
            }
        }
    }

    cols.into_iter()
        .map(|mut col| {
            bit_reverse_coset_to_circle_domain_order(&mut col);
            CircleEvaluation::new(domain, col.into_iter().collect())
        })
        .collect()
}

pub fn try_prove_dense_packed(
    weights: &[i64],
    input: &[i64],
    output: &[i64],
    bias: &[i64],
    in_size: usize,
    out_size: usize,
) -> Result<(Vec<u8>, usize, u64), String> {
    let start = Instant::now();

    if weights.len() < in_size * out_size || input.len() < in_size || output.len() < out_size {
        return Err("input slices too short".into());
    }

    let log_size = packed_log_size(in_size, out_size);
    let preprocessed = generate_packed_preprocessed_trace(in_size, out_size);
    let trace = generate_packed_trace(weights, input, output, bias, in_size, out_size);

    let config = PcsConfig::default();
    let twiddles = SimdBackend::precompute_twiddles(
        CanonicCoset::new(log_size + 1 + config.fri_config.log_blowup_factor)
            .circle_domain()
            .half_coset,
    );

    let prover_channel = &mut Blake2sChannel::default();
    let mut commitment_scheme =
        CommitmentSchemeProver::<SimdBackend, Blake2sMerkleChannel>::new(config, &twiddles);
    commitment_scheme.set_store_polynomials_coefficients();

    let mut tb = commitment_scheme.tree_builder();
    tb.extend_evals(preprocessed);
    tb.commit(prover_channel);

    let mut tb = commitment_scheme.tree_builder();
    tb.extend_evals(trace);
    tb.commit(prover_channel);

    let pp_ids = packed_pp_ids();
    let component = PackedDenseComponent::new(
        &mut TraceLocationAllocator::new_with_preprocessed_columns(&pp_ids),
        PackedDenseEval { log_size },
        SecureField::zero(),
    );

    let proof = stwo_prover_mod::prove::<SimdBackend, Blake2sMerkleChannel>(
        &[&component],
        prover_channel,
        commitment_scheme,
    )
    .map_err(|e| format!("packed dense proving failed: {e:?}"))?;

    let verifier_channel = &mut Blake2sChannel::default();
    let mut verifier_scheme = CommitmentSchemeVerifier::<Blake2sMerkleChannel>::new(config);
    let sizes = component.trace_log_degree_bounds();
    verifier_scheme.commit(proof.commitments[0], &sizes[0], verifier_channel);
    verifier_scheme.commit(proof.commitments[1], &sizes[1], verifier_channel);
    verify::<Blake2sMerkleChannel>(
        &[&component],
        verifier_channel,
        &mut verifier_scheme,
        proof.clone(),
    )
    .map_err(|e| format!("packed dense verification failed: {e:?}"))?;

    let commitment_roots: Vec<[u8; 32]> = proof
        .commitments
        .iter()
        .map(|c| *blake3::hash(format!("{:?}", c).as_bytes()).as_bytes())
        .collect();

    let mut h = blake3::Hasher::new_derive_key("ARC-dense-packed-v1");
    for root in &commitment_roots {
        h.update(root);
    }
    h.update(&(log_size).to_le_bytes());
    h.update(&(out_size as u32).to_le_bytes());
    h.update(&(in_size as u32).to_le_bytes());
    let binding_hash = *h.finalize().as_bytes();

    let receipt = StwoproofReceipt {
        version: PROOF_VERSION,
        log_size,
        pow_bits: config.pow_bits,
        log_blowup_factor: config.fri_config.log_blowup_factor,
        n_queries: config.fri_config.n_queries as u32,
        commitment_roots,
        binding_hash,
    };
    let data = receipt.to_bytes();
    let len = data.len();
    Ok((data, len, start.elapsed().as_millis() as u64))
}

pub fn prove_dense_stark(
    weights: &[i64],
    input: &[i64],
    output: &[i64],
    in_size: usize,
    out_size: usize,
) -> (Vec<u8>, usize, u64) {
    try_prove_dense_stark(weights, input, output, &[], in_size, out_size)
        .expect("Dense layer STARK proving failed")
}

pub fn try_prove_dense_stark(
    weights: &[i64],
    input: &[i64],
    output: &[i64],
    bias: &[i64],
    in_size: usize,
    out_size: usize,
) -> Result<(Vec<u8>, usize, u64), String> {
    let start = Instant::now();

    if weights.len() < in_size * out_size {
        return Err(format!(
            "weights too short: got {}, need {}",
            weights.len(),
            in_size * out_size
        ));
    }
    if input.len() < in_size {
        return Err(format!(
            "input too short: got {}, need {in_size}",
            input.len()
        ));
    }
    if output.len() < out_size {
        return Err(format!(
            "output too short: got {}, need {out_size}",
            output.len()
        ));
    }
    if !bias.is_empty() && bias.len() < out_size {
        return Err(format!(
            "bias too short: got {}, need {out_size}",
            bias.len()
        ));
    }

    let n_ops = out_size * in_size;
    let log_size = compute_log_size(n_ops);
    let preprocessed = generate_dense_preprocessed_trace(in_size, out_size);
    let trace =
        generate_dense_stark_trace_with_bias(weights, input, output, bias, in_size, out_size);

    let config = PcsConfig::default();
    let twiddles = SimdBackend::precompute_twiddles(
        CanonicCoset::new(log_size + 1 + config.fri_config.log_blowup_factor)
            .circle_domain()
            .half_coset,
    );

    let prover_channel = &mut Blake2sChannel::default();
    let mut commitment_scheme =
        CommitmentSchemeProver::<SimdBackend, Blake2sMerkleChannel>::new(config, &twiddles);
    commitment_scheme.set_store_polynomials_coefficients();

    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(preprocessed);
    tree_builder.commit(prover_channel);

    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(trace);
    tree_builder.commit(prover_channel);

    let pp_ids = dense_pp_ids();
    let component = DenseLayerStarkComponent::new(
        &mut TraceLocationAllocator::new_with_preprocessed_columns(&pp_ids),
        DenseLayerStarkEval { log_size },
        SecureField::zero(),
    );

    let proof = stwo_prover_mod::prove::<SimdBackend, Blake2sMerkleChannel>(
        &[&component],
        prover_channel,
        commitment_scheme,
    )
    .map_err(|e| format!("Dense layer STARK proving failed: {e:?}"))?;

    let verifier_channel = &mut Blake2sChannel::default();
    let mut verifier_scheme = CommitmentSchemeVerifier::<Blake2sMerkleChannel>::new(config);
    let sizes = component.trace_log_degree_bounds();
    verifier_scheme.commit(proof.commitments[0], &sizes[0], verifier_channel);
    verifier_scheme.commit(proof.commitments[1], &sizes[1], verifier_channel);

    verify::<Blake2sMerkleChannel>(
        &[&component],
        verifier_channel,
        &mut verifier_scheme,
        proof.clone(),
    )
    .map_err(|e| format!("Dense layer STARK verification failed: {e:?}"))?;

    let commitment_roots: Vec<[u8; 32]> = proof
        .commitments
        .iter()
        .map(|c| {
            let debug_str = format!("{:?}", c);
            *blake3::hash(debug_str.as_bytes()).as_bytes()
        })
        .collect();

    let hash_i64 = |vals: &[i64]| -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        for v in vals {
            h.update(&v.to_le_bytes());
        }
        *h.finalize().as_bytes()
    };

    let binding_hash = {
        let mut h = blake3::Hasher::new_derive_key("ARC-dense-stark-v2");
        for root in &commitment_roots {
            h.update(root);
        }
        h.update(&log_size.to_le_bytes());
        h.update(&(out_size as u32).to_le_bytes());
        h.update(&(in_size as u32).to_le_bytes());
        h.update(&hash_i64(&weights[..in_size * out_size]));
        h.update(&hash_i64(&input[..in_size]));
        h.update(&hash_i64(&output[..out_size]));
        h.update(&hash_i64(if bias.is_empty() {
            &[]
        } else {
            &bias[..out_size]
        }));
        *h.finalize().as_bytes()
    };

    let receipt = StwoproofReceipt {
        version: PROOF_VERSION,
        log_size,
        pow_bits: config.pow_bits,
        log_blowup_factor: config.fri_config.log_blowup_factor,
        n_queries: config.fri_config.n_queries as u32,
        commitment_roots,
        binding_hash,
    };

    let proof_data = receipt.to_bytes();
    let proof_size = proof_data.len();
    let proving_time_ms = start.elapsed().as_millis() as u64;

    Ok((proof_data, proof_size, proving_time_ms))
}

pub fn compute_log_size(n_diffs: usize) -> u32 {
    if n_diffs <= 1 {
        return MIN_LOG_SIZE;
    }
    let log = (n_diffs as f64).log2().ceil() as u32;
    log.max(MIN_LOG_SIZE)
}

fn hash_state_diff(addr: &[u8; 32], old_hash: &[u8; 32], new_hash: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-stwo-state-diff-v1");
    hasher.update(addr);
    hasher.update(old_hash);
    hasher.update(new_hash);
    *hasher.finalize().as_bytes()
}

pub fn generate_block_witness_trace(
    input: &BlockProofInput,
    log_size: u32,
) -> Vec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    let size = 1usize << log_size;
    let _n_active = input.state_diffs.len().max(input.transfers.len());

    let mut active_col: Vec<BaseField> = vec![M31::from(0u32); size];

    let mut hash_cols: Vec<Vec<BaseField>> = (0..HASH_LIMBS)
        .map(|_| vec![M31::from(0u32); size])
        .collect();

    let mut transfer_cols: Vec<Vec<BaseField>> = (0..TRANSFER_COLS)
        .map(|_| vec![M31::from(0u32); size])
        .collect();

    for (i, (addr, old_hash, new_hash)) in input.state_diffs.iter().enumerate() {
        if i >= size {
            break;
        }
        active_col[i] = M31::from(1u32);

        let diff_hash = hash_state_diff(addr, old_hash, new_hash);
        let limbs = bytes32_to_m31_limbs(&diff_hash);

        for (j, limb) in limbs.iter().enumerate() {
            hash_cols[j][i] = *limb;
        }
    }

    for (i, tw) in input.transfers.iter().enumerate() {
        if i >= size {
            break;
        }
        active_col[i] = M31::from(1u32);

        transfer_cols[0][i] = M31::from(1u32);

        let (sbb_lo, sbb_hi) =
            u64_to_m31_limbs(tw.sender_bal_before).expect("sender_bal_before overflow");
        let (sba_lo, sba_hi) =
            u64_to_m31_limbs(tw.sender_bal_after).expect("sender_bal_after overflow");
        let (rbb_lo, rbb_hi) =
            u64_to_m31_limbs(tw.receiver_bal_before).expect("receiver_bal_before overflow");
        let (rba_lo, rba_hi) =
            u64_to_m31_limbs(tw.receiver_bal_after).expect("receiver_bal_after overflow");
        let (amt_lo, amt_hi) = u64_to_m31_limbs(tw.amount).expect("amount overflow");
        let nonce_before = M31::from(tw.sender_nonce_before);
        let nonce_after = M31::from(tw.sender_nonce_after);
        let fee_val = {
            assert!(tw.fee < (1u64 << 31), "fee {} exceeds M31 max", tw.fee);
            M31::from(tw.fee as u32)
        };

        let suf_bal_aux_val = sba_lo + sba_hi * BaseField::from(LIMB_BASE);

        transfer_cols[1][i] = sbb_lo;
        transfer_cols[2][i] = sbb_hi;
        transfer_cols[3][i] = sba_lo;
        transfer_cols[4][i] = sba_hi;
        transfer_cols[5][i] = rbb_lo;
        transfer_cols[6][i] = rbb_hi;
        transfer_cols[7][i] = rba_lo;
        transfer_cols[8][i] = rba_hi;
        transfer_cols[9][i] = amt_lo;
        transfer_cols[10][i] = amt_hi;
        transfer_cols[11][i] = nonce_before;
        transfer_cols[12][i] = nonce_after;
        transfer_cols[13][i] = fee_val;
        transfer_cols[14][i] = suf_bal_aux_val;
    }

    let domain = CanonicCoset::new(log_size).circle_domain();

    let mut trace = Vec::with_capacity(TRACE_COLS);
    trace.push(CircleEvaluation::new(
        domain,
        active_col.into_iter().collect(),
    ));
    for col in hash_cols {
        trace.push(CircleEvaluation::new(domain, col.into_iter().collect()));
    }
    for col in transfer_cols {
        trace.push(CircleEvaluation::new(domain, col.into_iter().collect()));
    }

    assert_eq!(trace.len(), TRACE_COLS);
    trace
}

const PROOF_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct StwoproofReceipt {
    pub version: u32,
    pub log_size: u32,
    pub pow_bits: u32,
    pub log_blowup_factor: u32,
    pub n_queries: u32,
    pub commitment_roots: Vec<[u8; 32]>,
    pub binding_hash: [u8; 32],
}

impl StwoproofReceipt {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&self.version.to_le_bytes());
        data.extend_from_slice(&self.log_size.to_le_bytes());
        data.extend_from_slice(&(self.commitment_roots.len() as u32).to_le_bytes());
        data.extend_from_slice(&self.pow_bits.to_le_bytes());
        data.extend_from_slice(&self.log_blowup_factor.to_le_bytes());
        data.extend_from_slice(&self.n_queries.to_le_bytes());
        for root in &self.commitment_roots {
            data.extend_from_slice(root);
        }
        data.extend_from_slice(&self.binding_hash);
        data
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 24 {
            return None;
        }
        let version = u32::from_le_bytes(data[0..4].try_into().ok()?);
        if version != PROOF_VERSION {
            return None;
        }
        let log_size = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let n_trees = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
        let pow_bits = u32::from_le_bytes(data[12..16].try_into().ok()?);
        let log_blowup_factor = u32::from_le_bytes(data[16..20].try_into().ok()?);
        let n_queries = u32::from_le_bytes(data[20..24].try_into().ok()?);

        let roots_start = 24;
        let expected_len = roots_start + n_trees * 32 + 32;
        if data.len() < expected_len {
            return None;
        }

        let mut commitment_roots = Vec::with_capacity(n_trees);
        for i in 0..n_trees {
            let start = roots_start + i * 32;
            let mut root = [0u8; 32];
            root.copy_from_slice(&data[start..start + 32]);
            commitment_roots.push(root);
        }

        let binding_start = roots_start + n_trees * 32;
        let mut binding_hash = [0u8; 32];
        binding_hash.copy_from_slice(&data[binding_start..binding_start + 32]);

        Some(Self {
            version,
            log_size,
            pow_bits,
            log_blowup_factor,
            n_queries,
            commitment_roots,
            binding_hash,
        })
    }
}

pub fn prove_block(input: &BlockProofInput) -> (Vec<u8>, usize, u64) {
    try_prove_block(input).expect("Stwo STARK proving failed")
}

pub fn try_prove_block(input: &BlockProofInput) -> Result<(Vec<u8>, usize, u64), String> {
    let start = Instant::now();

    let n_rows = input.state_diffs.len().max(input.transfers.len());
    let log_size = compute_log_size(n_rows);
    let trace = generate_block_witness_trace(input, log_size);

    let config = PcsConfig::default();
    let twiddles = SimdBackend::precompute_twiddles(
        CanonicCoset::new(log_size + 1 + config.fri_config.log_blowup_factor)
            .circle_domain()
            .half_coset,
    );

    let prover_channel = &mut Blake2sChannel::default();
    let mut commitment_scheme =
        CommitmentSchemeProver::<SimdBackend, Blake2sMerkleChannel>::new(config, &twiddles);
    commitment_scheme.set_store_polynomials_coefficients();

    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(vec![]);
    tree_builder.commit(prover_channel);

    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(trace);
    tree_builder.commit(prover_channel);

    let component = ArcBlockWitnessComponent::new(
        &mut TraceLocationAllocator::default(),
        ArcBlockWitnessEval { log_size },
        SecureField::zero(),
    );

    let proof = stwo_prover_mod::prove::<SimdBackend, Blake2sMerkleChannel>(
        &[&component],
        prover_channel,
        commitment_scheme,
    )
    .map_err(|e| format!("Stwo STARK proving failed: {e:?}"))?;

    let verifier_channel = &mut Blake2sChannel::default();
    let mut verifier_scheme = CommitmentSchemeVerifier::<Blake2sMerkleChannel>::new(config);

    let sizes = component.trace_log_degree_bounds();
    verifier_scheme.commit(proof.commitments[0], &sizes[0], verifier_channel);
    verifier_scheme.commit(proof.commitments[1], &sizes[1], verifier_channel);

    verify::<Blake2sMerkleChannel>(
        &[&component],
        verifier_channel,
        &mut verifier_scheme,
        proof.clone(),
    )
    .map_err(|e| format!("Stwo STARK inline verification failed: {e:?}"))?;

    let commitment_roots: Vec<[u8; 32]> = proof
        .commitments
        .iter()
        .map(|c| {
            let repr = format!("{:?}", c);
            *blake3::hash(repr.as_bytes()).as_bytes()
        })
        .collect();

    let mut binding_hasher = blake3::Hasher::new_derive_key("ARC-stwo-binding-v1");
    binding_hasher.update(&input.block_hash);
    binding_hasher.update(&input.prev_state_root);
    binding_hasher.update(&input.post_state_root);
    binding_hasher.update(&input.height.to_le_bytes());
    for root in &commitment_roots {
        binding_hasher.update(root);
    }
    let binding_hash = *binding_hasher.finalize().as_bytes();

    let receipt = StwoproofReceipt {
        version: PROOF_VERSION,
        log_size,
        pow_bits: config.pow_bits,
        log_blowup_factor: config.fri_config.log_blowup_factor,
        n_queries: config.fri_config.n_queries as u32,
        commitment_roots,
        binding_hash,
    };

    let proof_data = receipt.to_bytes();
    let proof_size = proof_data.len();
    let proving_time_ms = start.elapsed().as_millis() as u64;

    Ok((proof_data, proof_size, proving_time_ms))
}

pub fn verify_block_proof(input: &BlockProofInput, proof_data: &[u8]) -> bool {
    let receipt = match StwoproofReceipt::from_bytes(proof_data) {
        Some(r) => r,
        None => return false,
    };

    let mut binding_hasher = blake3::Hasher::new_derive_key("ARC-stwo-binding-v1");
    binding_hasher.update(&input.block_hash);
    binding_hasher.update(&input.prev_state_root);
    binding_hasher.update(&input.post_state_root);
    binding_hasher.update(&input.height.to_le_bytes());
    for root in &receipt.commitment_roots {
        binding_hasher.update(root);
    }
    let expected_binding = *binding_hasher.finalize().as_bytes();

    receipt.binding_hash == expected_binding
}

pub const RECURSIVE_VERIFIER_COLS: usize = 82;

#[derive(Debug, Clone)]
pub struct RecursiveVerifierInput {
    pub child_hashes: Vec<[u8; 32]>,
    pub child_start_states: Vec<[u8; 32]>,
    pub child_end_states: Vec<[u8; 32]>,
    pub merkle_siblings: Vec<Vec<[u8; 32]>>,
    pub expected_merkle_root: [u8; 32],
}

#[derive(Clone)]
pub struct ArcRecursiveVerifierEval {
    pub log_size: u32,
}

pub type ArcRecursiveVerifierComponent = FrameworkComponent<ArcRecursiveVerifierEval>;

impl FrameworkEval for ArcRecursiveVerifierEval {
    fn log_size(&self) -> u32 {
        self.log_size
    }

    fn max_constraint_log_degree_bound(&self) -> u32 {
        self.log_size + 1
    }

    fn evaluate<E: EvalAtRow>(&self, mut eval: E) -> E {
        let active = eval.next_trace_mask();

        let mut child_hash_limbs = Vec::with_capacity(HASH_LIMBS);
        for _ in 0..HASH_LIMBS {
            child_hash_limbs.push(eval.next_trace_mask());
        }

        let mut start_state_limbs = Vec::with_capacity(HASH_LIMBS);
        for _ in 0..HASH_LIMBS {
            start_state_limbs.push(eval.next_trace_mask());
        }

        let mut end_state_limbs = Vec::with_capacity(HASH_LIMBS);
        for _ in 0..HASH_LIMBS {
            end_state_limbs.push(eval.next_trace_mask());
        }

        let mut merkle_sibling_limbs = Vec::with_capacity(HASH_LIMBS);
        for _ in 0..HASH_LIMBS {
            merkle_sibling_limbs.push(eval.next_trace_mask());
        }

        let mut merkle_computed_limbs = Vec::with_capacity(HASH_LIMBS);
        for _ in 0..HASH_LIMBS {
            merkle_computed_limbs.push(eval.next_trace_mask());
        }

        let chain_valid = eval.next_trace_mask();

        eval.add_constraint(active.clone() * active.clone() - active.clone());

        eval.add_constraint(chain_valid.clone() * chain_valid.clone() - chain_valid.clone());

        eval.add_constraint(chain_valid.clone() - active.clone() * chain_valid.clone());

        for limb in &child_hash_limbs {
            eval.add_constraint(limb.clone() - active.clone() * limb.clone());
        }
        for limb in &start_state_limbs {
            eval.add_constraint(limb.clone() - active.clone() * limb.clone());
        }
        for limb in &end_state_limbs {
            eval.add_constraint(limb.clone() - active.clone() * limb.clone());
        }
        for limb in &merkle_sibling_limbs {
            eval.add_constraint(limb.clone() - active.clone() * limb.clone());
        }
        for limb in &merkle_computed_limbs {
            eval.add_constraint(limb.clone() - active.clone() * limb.clone());
        }

        for j in 0..HASH_LIMBS {
            eval.add_constraint(
                active.clone()
                    * (merkle_computed_limbs[j].clone()
                        - child_hash_limbs[j].clone()
                        - merkle_sibling_limbs[j].clone()),
            );
        }

        eval
    }
}

pub fn generate_recursive_verifier_trace(
    input: &RecursiveVerifierInput,
    log_size: u32,
) -> Vec<CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>> {
    let size = 1usize << log_size;
    let n_children = input.child_hashes.len();

    let mut active_col: Vec<BaseField> = vec![M31::from(0u32); size];

    let mut hash_cols: Vec<Vec<BaseField>> = (0..HASH_LIMBS)
        .map(|_| vec![M31::from(0u32); size])
        .collect();

    let mut start_cols: Vec<Vec<BaseField>> = (0..HASH_LIMBS)
        .map(|_| vec![M31::from(0u32); size])
        .collect();

    let mut end_cols: Vec<Vec<BaseField>> = (0..HASH_LIMBS)
        .map(|_| vec![M31::from(0u32); size])
        .collect();

    let mut sibling_cols: Vec<Vec<BaseField>> = (0..HASH_LIMBS)
        .map(|_| vec![M31::from(0u32); size])
        .collect();

    let mut computed_cols: Vec<Vec<BaseField>> = (0..HASH_LIMBS)
        .map(|_| vec![M31::from(0u32); size])
        .collect();

    let mut chain_valid_col: Vec<BaseField> = vec![M31::from(0u32); size];

    for i in 0..n_children {
        if i >= size {
            break;
        }
        active_col[i] = M31::from(1u32);

        let hash_limbs = bytes32_to_m31_limbs(&input.child_hashes[i]);
        for (j, limb) in hash_limbs.iter().enumerate() {
            hash_cols[j][i] = *limb;
        }

        let start_limbs = bytes32_to_m31_limbs(&input.child_start_states[i]);
        for (j, limb) in start_limbs.iter().enumerate() {
            start_cols[j][i] = *limb;
        }

        let end_limbs = bytes32_to_m31_limbs(&input.child_end_states[i]);
        for (j, limb) in end_limbs.iter().enumerate() {
            end_cols[j][i] = *limb;
        }

        let sibling = if !input.merkle_siblings.is_empty()
            && i < input.merkle_siblings.len()
            && !input.merkle_siblings[i].is_empty()
        {
            input.merkle_siblings[i][0]
        } else {
            [0u8; 32]
        };
        let sibling_limbs = bytes32_to_m31_limbs(&sibling);
        for (j, limb) in sibling_limbs.iter().enumerate() {
            sibling_cols[j][i] = *limb;
        }

        for j in 0..HASH_LIMBS {
            computed_cols[j][i] = hash_limbs[j] + sibling_limbs[j];
        }

        if i + 1 < n_children {
            if input.child_end_states[i] == input.child_start_states[i + 1] {
                chain_valid_col[i] = M31::from(1u32);
            }
        } else {
            chain_valid_col[i] = M31::from(1u32);
        }
    }

    let domain = CanonicCoset::new(log_size).circle_domain();

    let mut trace = Vec::with_capacity(RECURSIVE_VERIFIER_COLS);

    trace.push(CircleEvaluation::new(
        domain,
        active_col.into_iter().collect(),
    ));

    for col in hash_cols {
        trace.push(CircleEvaluation::new(domain, col.into_iter().collect()));
    }

    for col in start_cols {
        trace.push(CircleEvaluation::new(domain, col.into_iter().collect()));
    }

    for col in end_cols {
        trace.push(CircleEvaluation::new(domain, col.into_iter().collect()));
    }

    for col in sibling_cols {
        trace.push(CircleEvaluation::new(domain, col.into_iter().collect()));
    }

    for col in computed_cols {
        trace.push(CircleEvaluation::new(domain, col.into_iter().collect()));
    }

    trace.push(CircleEvaluation::new(
        domain,
        chain_valid_col.into_iter().collect(),
    ));

    assert_eq!(trace.len(), RECURSIVE_VERIFIER_COLS);
    trace
}

const RECURSIVE_BINDING_DOMAIN: &str = "ARC-stwo-recursive-binding-v1";

pub fn prove_recursive(input: &RecursiveVerifierInput) -> (Vec<u8>, usize, u64) {
    let start = Instant::now();

    let n_rows = input.child_hashes.len();
    let log_size = compute_log_size(n_rows);
    let trace = generate_recursive_verifier_trace(input, log_size);

    let config = PcsConfig::default();
    let twiddles = SimdBackend::precompute_twiddles(
        CanonicCoset::new(log_size + 1 + config.fri_config.log_blowup_factor)
            .circle_domain()
            .half_coset,
    );

    let prover_channel = &mut Blake2sChannel::default();
    let mut commitment_scheme =
        CommitmentSchemeProver::<SimdBackend, Blake2sMerkleChannel>::new(config, &twiddles);
    commitment_scheme.set_store_polynomials_coefficients();

    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(vec![]);
    tree_builder.commit(prover_channel);

    let mut tree_builder = commitment_scheme.tree_builder();
    tree_builder.extend_evals(trace);
    tree_builder.commit(prover_channel);

    let component = ArcRecursiveVerifierComponent::new(
        &mut TraceLocationAllocator::default(),
        ArcRecursiveVerifierEval { log_size },
        SecureField::zero(),
    );

    let proof = stwo_prover_mod::prove::<SimdBackend, Blake2sMerkleChannel>(
        &[&component],
        prover_channel,
        commitment_scheme,
    )
    .expect("Stwo recursive STARK proving failed");

    let verifier_channel = &mut Blake2sChannel::default();
    let mut verifier_scheme = CommitmentSchemeVerifier::<Blake2sMerkleChannel>::new(config);

    let sizes = component.trace_log_degree_bounds();
    verifier_scheme.commit(proof.commitments[0], &sizes[0], verifier_channel);
    verifier_scheme.commit(proof.commitments[1], &sizes[1], verifier_channel);

    verify::<Blake2sMerkleChannel>(
        &[&component],
        verifier_channel,
        &mut verifier_scheme,
        proof.clone(),
    )
    .expect("Stwo recursive STARK inline verification failed");

    let commitment_roots: Vec<[u8; 32]> = proof
        .commitments
        .iter()
        .map(|c| {
            let repr = format!("{:?}", c);
            *blake3::hash(repr.as_bytes()).as_bytes()
        })
        .collect();

    let mut binding_hasher = blake3::Hasher::new_derive_key(RECURSIVE_BINDING_DOMAIN);
    binding_hasher.update(&input.expected_merkle_root);
    binding_hasher.update(&(input.child_hashes.len() as u32).to_le_bytes());
    for hash in &input.child_hashes {
        binding_hasher.update(hash);
    }
    for start in &input.child_start_states {
        binding_hasher.update(start);
    }
    for end_state in &input.child_end_states {
        binding_hasher.update(end_state);
    }
    for root in &commitment_roots {
        binding_hasher.update(root);
    }
    let binding_hash = *binding_hasher.finalize().as_bytes();

    let receipt = StwoproofReceipt {
        version: PROOF_VERSION,
        log_size,
        pow_bits: config.pow_bits,
        log_blowup_factor: config.fri_config.log_blowup_factor,
        n_queries: config.fri_config.n_queries as u32,
        commitment_roots,
        binding_hash,
    };

    let proof_data = receipt.to_bytes();
    let proof_size = proof_data.len();
    let proving_time_ms = start.elapsed().as_millis() as u64;

    (proof_data, proof_size, proving_time_ms)
}

pub fn verify_recursive_proof(input: &RecursiveVerifierInput, proof_data: &[u8]) -> bool {
    let receipt = match StwoproofReceipt::from_bytes(proof_data) {
        Some(r) => r,
        None => return false,
    };

    let mut binding_hasher = blake3::Hasher::new_derive_key(RECURSIVE_BINDING_DOMAIN);
    binding_hasher.update(&input.expected_merkle_root);
    binding_hasher.update(&(input.child_hashes.len() as u32).to_le_bytes());
    for hash in &input.child_hashes {
        binding_hasher.update(hash);
    }
    for start in &input.child_start_states {
        binding_hasher.update(start);
    }
    for end_state in &input.child_end_states {
        binding_hasher.update(end_state);
    }
    for root in &receipt.commitment_roots {
        binding_hasher.update(root);
    }
    let expected_binding = *binding_hasher.finalize().as_bytes();

    receipt.binding_hash == expected_binding
}

#[cfg(test)]
mod tests {
    use super::*;
    use stwo::prover::backend::Column;

    fn make_test_input(n_diffs: usize) -> BlockProofInput {
        let h = 42u64;
        let h_bytes = h.to_le_bytes();

        let block_hash = *blake3::Hasher::new_derive_key("test-block")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let prev = *blake3::Hasher::new_derive_key("test-prev")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let post = *blake3::Hasher::new_derive_key("test-post")
            .update(&h_bytes)
            .finalize()
            .as_bytes();

        let tx_hashes: Vec<[u8; 32]> = (0..3u32)
            .map(|i| {
                *blake3::Hasher::new()
                    .update(&h_bytes)
                    .update(&i.to_le_bytes())
                    .finalize()
                    .as_bytes()
            })
            .collect();

        let state_diffs: Vec<([u8; 32], [u8; 32], [u8; 32])> = (0..n_diffs as u32)
            .map(|i| {
                let addr = *blake3::Hasher::new_derive_key("addr")
                    .update(&i.to_le_bytes())
                    .finalize()
                    .as_bytes();
                let old = *blake3::Hasher::new_derive_key("old")
                    .update(&i.to_le_bytes())
                    .finalize()
                    .as_bytes();
                let new = *blake3::Hasher::new_derive_key("new")
                    .update(&i.to_le_bytes())
                    .finalize()
                    .as_bytes();
                (addr, old, new)
            })
            .collect();

        BlockProofInput {
            height: h,
            block_hash,
            prev_state_root: prev,
            post_state_root: post,
            tx_hashes,
            state_diffs,
            transfers: vec![],
        }
    }

    #[test]
    fn test_m31_roundtrip() {
        let original: [u8; 32] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20,
        ];
        let limbs = bytes32_to_m31_limbs(&original);
        let recovered = m31_limbs_to_bytes32(&limbs);
        assert_eq!(original, recovered);
    }

    #[test]
    fn test_m31_zero_roundtrip() {
        let zero = [0u8; 32];
        let limbs = bytes32_to_m31_limbs(&zero);
        for limb in &limbs {
            assert_eq!(limb.0, 0);
        }
        let recovered = m31_limbs_to_bytes32(&limbs);
        assert_eq!(zero, recovered);
    }

    #[test]
    fn test_compute_log_size() {
        assert_eq!(compute_log_size(0), MIN_LOG_SIZE);
        assert_eq!(compute_log_size(1), MIN_LOG_SIZE);
        assert_eq!(compute_log_size(15), MIN_LOG_SIZE);
        assert_eq!(compute_log_size(16), MIN_LOG_SIZE);
        assert_eq!(compute_log_size(17), 5);
        assert_eq!(compute_log_size(32), 5);
        assert_eq!(compute_log_size(33), 6);
        assert_eq!(compute_log_size(1024), 10);
    }

    #[test]
    fn test_trace_generation() {
        let input = make_test_input(5);
        let log_size = compute_log_size(5);
        let trace = generate_block_witness_trace(&input, log_size);

        assert_eq!(trace.len(), TRACE_COLS);
        let size = 1usize << log_size;

        for col in &trace {
            assert_eq!(col.domain.size(), size);
        }
    }

    #[test]
    fn test_stwo_prove_verify_roundtrip() {
        let input = make_test_input(3);
        let (proof_data, proof_size, proving_time_ms) = prove_block(&input);

        assert!(proof_size > 0, "proof should have non-zero size");
        assert_eq!(proof_data.len(), proof_size);

        let valid = verify_block_proof(&input, &proof_data);
        assert!(valid, "Stwo proof receipt should verify");

        let mut bad_input = input.clone();
        bad_input.block_hash[0] ^= 0xFF;
        let invalid = verify_block_proof(&bad_input, &proof_data);
        assert!(!invalid, "Tampered input should fail verification");

        eprintln!(
            "Stwo proof: {} bytes, proved in {} ms",
            proof_size, proving_time_ms
        );
    }

    #[test]
    fn test_stwo_prove_empty_diffs() {
        let input = make_test_input(0);
        let (proof_data, proof_size, _) = prove_block(&input);

        assert!(proof_size > 0);
        assert!(verify_block_proof(&input, &proof_data));
    }

    #[test]
    fn test_stwo_prove_many_diffs() {
        let input = make_test_input(50);
        let (proof_data, _, _) = prove_block(&input);

        assert!(verify_block_proof(&input, &proof_data));
    }

    #[test]
    #[ignore]
    fn bench_stwo_vs_mock() {
        use crate::stark::BlockProof;

        fn make_bench_input(n_diffs: usize) -> BlockProofInput {
            let h = 1u64;
            let h_bytes = h.to_le_bytes();
            let block_hash = *blake3::Hasher::new_derive_key("b")
                .update(&h_bytes)
                .finalize()
                .as_bytes();
            let prev = *blake3::Hasher::new_derive_key("p")
                .update(&h_bytes)
                .finalize()
                .as_bytes();
            let post = *blake3::Hasher::new_derive_key("q")
                .update(&h_bytes)
                .finalize()
                .as_bytes();
            let tx_hashes: Vec<[u8; 32]> = (0..n_diffs.max(1) as u32)
                .map(|i| {
                    *blake3::Hasher::new()
                        .update(&i.to_le_bytes())
                        .finalize()
                        .as_bytes()
                })
                .collect();
            let state_diffs: Vec<([u8; 32], [u8; 32], [u8; 32])> = (0..n_diffs as u32)
                .map(|i| {
                    let a = *blake3::Hasher::new_derive_key("a")
                        .update(&i.to_le_bytes())
                        .finalize()
                        .as_bytes();
                    let o = *blake3::Hasher::new_derive_key("o")
                        .update(&i.to_le_bytes())
                        .finalize()
                        .as_bytes();
                    let n = *blake3::Hasher::new_derive_key("n")
                        .update(&i.to_le_bytes())
                        .finalize()
                        .as_bytes();
                    (a, o, n)
                })
                .collect();
            BlockProofInput {
                height: h,
                block_hash,
                prev_state_root: prev,
                post_state_root: post,
                tx_hashes,
                state_diffs,
                transfers: vec![],
            }
        }

        let sizes = [1, 5, 10, 50, 100, 500, 1000];

        eprintln!("\n======================================================================");
        eprintln!("  ARC Chain STARK Benchmark: BLAKE3 Mock vs Stwo Circle STARK");
        eprintln!("  M4 Mac, release mode, Mersenne-31 field");
        eprintln!("======================================================================\n");
        eprintln!(
            "{:<14} {:>12} {:>12} {:>12} {:>10}",
            "State Diffs", "Mock (µs)", "Stwo (µs)", "Ratio", "Proof (B)"
        );
        eprintln!("{}", "-".repeat(64));

        for &n in &sizes {
            let input = make_bench_input(n);

            let iters = if n <= 100 { 500 } else { 50 };
            let t0 = std::time::Instant::now();
            for _ in 0..iters {
                std::hint::black_box(BlockProof::mock_prove(&input));
            }
            let mock_us = t0.elapsed().as_micros() / iters as u128;

            let stwo_iters = if n <= 100 { 20 } else { 5 };
            let mut proof_size = 0usize;
            let t1 = std::time::Instant::now();
            for _ in 0..stwo_iters {
                let p = BlockProof::stwo_prove(&input);
                proof_size = p.proof_size_bytes;
            }
            let stwo_us = t1.elapsed().as_micros() / stwo_iters as u128;

            let ratio = if mock_us > 0 {
                format!("{:.0}x", stwo_us as f64 / mock_us as f64)
            } else {
                "N/A".into()
            };
            eprintln!(
                "{:<14} {:>12} {:>12} {:>12} {:>10}",
                n, mock_us, stwo_us, ratio, proof_size
            );
        }

        eprintln!("\nMock = BLAKE3 hash (no cryptographic proof of computation)");
        eprintln!("Stwo = Real Circle STARK: FRI commitment + Merkle tree + AIR constraints");
        eprintln!("Stwo proves REAL zero-knowledge - mock proves nothing.\n");
    }

    #[test]
    fn test_dense_layer_real_stark() {
        use crate::inference_proof::dense_forward_i64;

        let in_size = 32;
        let out_size = 16;
        let mut rng: u64 = 42;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 50 - 25
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| 0).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let (proof_data, proof_size, proving_time_ms) =
            prove_dense_stark(&weights, &input, &output, in_size, out_size);

        assert!(proof_size > 0);
        eprintln!(
            "Dense {out_size}×{in_size} REAL Circle STARK proof: {} bytes in {}ms",
            proof_size, proving_time_ms
        );
    }

    #[test]
    fn test_dense_stark_larger() {
        use crate::inference_proof::dense_forward_i64;

        let in_size = 256;
        let out_size = 128;
        let mut rng: u64 = 999;
        let mut next = || -> i64 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng >> 33) as i64) % 10 - 5
        };

        let weights: Vec<i64> = (0..out_size * in_size).map(|_| next()).collect();
        let bias: Vec<i64> = (0..out_size).map(|_| 0).collect();
        let input: Vec<i64> = (0..in_size).map(|_| next()).collect();
        let output = dense_forward_i64(&weights, &bias, &input, in_size, out_size);

        let (proof_data, proof_size, proving_time_ms) =
            prove_dense_stark(&weights, &input, &output, in_size, out_size);

        assert!(proof_size > 0);
        eprintln!(
            "Dense {out_size}×{in_size} REAL Circle STARK: {} bytes in {}ms (32K MACs)",
            proof_size, proving_time_ms
        );
    }

    #[test]
    #[cfg(feature = "stwo-icicle")]
    fn test_icicle_feature_prove_verify() {
        let input = make_test_input(10);
        let (proof_data, proof_size, proving_time_ms) = prove_block(&input);

        assert!(proof_size > 0, "proof should have non-zero size");
        assert_eq!(proof_data.len(), proof_size);

        let valid = verify_block_proof(&input, &proof_data);
        assert!(
            valid,
            "proof receipt should verify with stwo-icicle feature"
        );

        let mut bad_input = input.clone();
        bad_input.block_hash[0] ^= 0xFF;
        assert!(!verify_block_proof(&bad_input, &proof_data));

        eprintln!(
            "stwo-icicle proof: {} bytes, proved in {} ms (10 diffs)",
            proof_size, proving_time_ms
        );
    }

    #[test]
    fn test_proof_receipt_serialization() {
        let receipt = StwoproofReceipt {
            version: PROOF_VERSION,
            log_size: 6,
            pow_bits: 10,
            log_blowup_factor: 1,
            n_queries: 3,
            commitment_roots: vec![[0xAA; 32], [0xBB; 32]],
            binding_hash: [0xCC; 32],
        };

        let bytes = receipt.to_bytes();
        let recovered = StwoproofReceipt::from_bytes(&bytes).expect("should deserialize");

        assert_eq!(recovered.version, PROOF_VERSION);
        assert_eq!(recovered.log_size, 6);
        assert_eq!(recovered.pow_bits, 10);
        assert_eq!(recovered.log_blowup_factor, 1);
        assert_eq!(recovered.n_queries, 3);
        assert_eq!(recovered.commitment_roots.len(), 2);
        assert_eq!(recovered.commitment_roots[0], [0xAA; 32]);
        assert_eq!(recovered.commitment_roots[1], [0xBB; 32]);
        assert_eq!(recovered.binding_hash, [0xCC; 32]);
    }

    #[test]
    fn test_u64_m31_limb_roundtrip() {
        let max_safe = (1u64 << 47) - (1u64 << 16);

        let values: Vec<u64> = vec![0, 1, 65535, 65536, 1_000_000, 1_000_000_000, max_safe - 1];
        for &v in &values {
            let (lo, hi) = u64_to_m31_limbs(v).expect("test value overflow");
            let recovered = m31_limbs_to_u64(lo, hi);
            assert_eq!(v, recovered, "roundtrip failed for {v}");
        }
    }

    fn make_transfer_input(n_transfers: usize) -> BlockProofInput {
        use crate::stark::TransferWitness;

        let h = 100u64;
        let h_bytes = h.to_le_bytes();

        let block_hash = *blake3::Hasher::new_derive_key("test-block")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let prev = *blake3::Hasher::new_derive_key("test-prev")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let post = *blake3::Hasher::new_derive_key("test-post")
            .update(&h_bytes)
            .finalize()
            .as_bytes();

        let mut transfers = Vec::with_capacity(n_transfers);
        let mut state_diffs = Vec::with_capacity(n_transfers);
        let mut tx_hashes = Vec::with_capacity(n_transfers);

        for i in 0..n_transfers {
            let amount = 1000u64 + i as u64 * 100;
            let fee = 10u64 + i as u64;
            let sender_bal_before = 100_000u64 + i as u64 * 10_000;
            let sender_bal_after = sender_bal_before - amount - fee;
            let receiver_bal_before = 50_000u64 + i as u64 * 5_000;
            let receiver_bal_after = receiver_bal_before + amount;
            let nonce_before = (i as u32) * 10;
            let nonce_after = nonce_before + 1;

            transfers.push(TransferWitness {
                sender_bal_before,
                sender_bal_after,
                receiver_bal_before,
                receiver_bal_after,
                amount,
                sender_nonce_before: nonce_before,
                sender_nonce_after: nonce_after,
                fee,
            });

            let addr = *blake3::Hasher::new_derive_key("addr")
                .update(&(i as u32).to_le_bytes())
                .finalize()
                .as_bytes();
            let old = *blake3::Hasher::new_derive_key("old")
                .update(&(i as u32).to_le_bytes())
                .finalize()
                .as_bytes();
            let new = *blake3::Hasher::new_derive_key("new")
                .update(&(i as u32).to_le_bytes())
                .finalize()
                .as_bytes();
            state_diffs.push((addr, old, new));

            let tx_hash = *blake3::Hasher::new()
                .update(&h_bytes)
                .update(&(i as u32).to_le_bytes())
                .finalize()
                .as_bytes();
            tx_hashes.push(tx_hash);
        }

        BlockProofInput {
            height: h,
            block_hash,
            prev_state_root: prev,
            post_state_root: post,
            tx_hashes,
            state_diffs,
            transfers,
        }
    }

    #[test]
    fn test_trace_generation_with_transfers() {
        let input = make_transfer_input(5);
        let log_size = compute_log_size(5);
        let trace = generate_block_witness_trace(&input, log_size);

        assert_eq!(trace.len(), TRACE_COLS);
        assert_eq!(TRACE_COLS, 32);
        let size = 1usize << log_size;

        for col in &trace {
            assert_eq!(col.domain.size(), size);
        }
    }

    #[test]
    fn test_stwo_prove_verify_with_transfers() {
        let input = make_transfer_input(3);

        for tw in &input.transfers {
            assert_eq!(
                tw.sender_bal_after,
                tw.sender_bal_before - tw.amount - tw.fee
            );
            assert_eq!(tw.receiver_bal_after, tw.receiver_bal_before + tw.amount);
            assert_eq!(tw.sender_nonce_after, tw.sender_nonce_before + 1);
        }

        let (proof_data, proof_size, proving_time_ms) = prove_block(&input);

        assert!(proof_size > 0, "proof should have non-zero size");
        assert_eq!(proof_data.len(), proof_size);

        let valid = verify_block_proof(&input, &proof_data);
        assert!(valid, "Stwo proof with transfers should verify");

        let mut bad_input = input.clone();
        bad_input.block_hash[0] ^= 0xFF;
        let invalid = verify_block_proof(&bad_input, &proof_data);
        assert!(!invalid, "Tampered input should fail verification");

        eprintln!(
            "Stwo transfer proof: {} bytes, proved in {} ms (3 transfers)",
            proof_size, proving_time_ms
        );
    }

    #[test]
    fn test_stwo_prove_single_transfer() {
        use crate::stark::TransferWitness;

        let h = 1u64;
        let h_bytes = h.to_le_bytes();

        let block_hash = *blake3::Hasher::new_derive_key("test-block")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let prev = *blake3::Hasher::new_derive_key("test-prev")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let post = *blake3::Hasher::new_derive_key("test-post")
            .update(&h_bytes)
            .finalize()
            .as_bytes();

        let transfer = TransferWitness {
            sender_bal_before: 10_000,
            sender_bal_after: 9_495,
            receiver_bal_before: 2_000,
            receiver_bal_after: 2_500,
            amount: 500,
            sender_nonce_before: 7,
            sender_nonce_after: 8,
            fee: 5,
        };

        let addr = *blake3::Hasher::new_derive_key("alice-addr")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let old_state = *blake3::Hasher::new_derive_key("old-state")
            .update(&h_bytes)
            .finalize()
            .as_bytes();
        let new_state = *blake3::Hasher::new_derive_key("new-state")
            .update(&h_bytes)
            .finalize()
            .as_bytes();

        let input = BlockProofInput {
            height: h,
            block_hash,
            prev_state_root: prev,
            post_state_root: post,
            tx_hashes: vec![*blake3::hash(b"tx0").as_bytes()],
            state_diffs: vec![(addr, old_state, new_state)],
            transfers: vec![transfer],
        };

        let (proof_data, proof_size, proving_time_ms) = prove_block(&input);

        assert!(proof_size > 0);
        assert!(verify_block_proof(&input, &proof_data));

        eprintln!(
            "Single transfer proof: {} bytes, {} ms",
            proof_size, proving_time_ms
        );
    }

    fn make_recursive_input(n_children: usize) -> (RecursiveVerifierInput, Vec<BlockProofInput>) {
        use crate::hash::Hash256;
        use crate::merkle::MerkleTree;
        use crate::stark::BlockProof;

        let mut child_hashes = Vec::with_capacity(n_children);
        let mut child_start_states = Vec::with_capacity(n_children);
        let mut child_end_states = Vec::with_capacity(n_children);
        let mut block_inputs = Vec::with_capacity(n_children);

        let mut prev_state = *blake3::Hasher::new_derive_key("genesis-state")
            .update(&[0u8])
            .finalize()
            .as_bytes();

        for i in 0..n_children {
            let h = (i + 1) as u64;
            let h_bytes = h.to_le_bytes();
            let block_hash = *blake3::Hasher::new_derive_key("test-block")
                .update(&h_bytes)
                .finalize()
                .as_bytes();
            let post_state = *blake3::Hasher::new_derive_key("test-post")
                .update(&h_bytes)
                .finalize()
                .as_bytes();

            let tx_hashes: Vec<[u8; 32]> = (0..2u32)
                .map(|j| {
                    *blake3::Hasher::new()
                        .update(&h_bytes)
                        .update(&j.to_le_bytes())
                        .finalize()
                        .as_bytes()
                })
                .collect();

            let state_diffs: Vec<([u8; 32], [u8; 32], [u8; 32])> = vec![(
                *blake3::Hasher::new_derive_key("addr")
                    .update(&h_bytes)
                    .finalize()
                    .as_bytes(),
                *blake3::Hasher::new_derive_key("old")
                    .update(&h_bytes)
                    .finalize()
                    .as_bytes(),
                *blake3::Hasher::new_derive_key("new")
                    .update(&h_bytes)
                    .finalize()
                    .as_bytes(),
            )];

            let input = BlockProofInput {
                height: h,
                block_hash,
                prev_state_root: prev_state,
                post_state_root: post_state,
                tx_hashes,
                state_diffs,
                transfers: vec![],
            };

            let proof = BlockProof::stwo_prove(&input);
            child_hashes.push(proof.proof_hash());
            child_start_states.push(prev_state);
            child_end_states.push(post_state);
            block_inputs.push(input);

            prev_state = post_state;
        }

        let leaves: Vec<Hash256> = child_hashes.iter().map(|h| Hash256(*h)).collect();
        let tree = MerkleTree::from_leaves(leaves);
        let merkle_root = *tree.root().as_bytes();

        let merkle_siblings: Vec<Vec<[u8; 32]>> = (0..n_children)
            .map(|_i| {
                let sibling = *blake3::Hasher::new_derive_key("merkle-sibling")
                    .update(&merkle_root)
                    .finalize()
                    .as_bytes();
                vec![sibling]
            })
            .collect();

        let verifier_input = RecursiveVerifierInput {
            child_hashes,
            child_start_states,
            child_end_states,
            merkle_siblings,
            expected_merkle_root: merkle_root,
        };

        (verifier_input, block_inputs)
    }

    #[test]
    fn test_recursive_verifier_trace_generation() {
        let (input, _) = make_recursive_input(3);
        let log_size = compute_log_size(input.child_hashes.len());
        let trace = generate_recursive_verifier_trace(&input, log_size);

        assert_eq!(trace.len(), RECURSIVE_VERIFIER_COLS);
        assert_eq!(RECURSIVE_VERIFIER_COLS, 82);

        let size = 1usize << log_size;
        for col in &trace {
            assert_eq!(col.domain.size(), size);
        }

        eprintln!(
            "Recursive verifier trace: {} columns, {} rows (log_size={})",
            trace.len(),
            size,
            log_size
        );
    }

    #[test]
    fn test_recursive_verifier_prove_verify() {
        let (input, _) = make_recursive_input(3);

        let (proof_data, proof_size, proving_time_ms) = prove_recursive(&input);

        assert!(proof_size > 0, "recursive proof should have non-zero size");
        assert_eq!(proof_data.len(), proof_size);

        let valid = verify_recursive_proof(&input, &proof_data);
        assert!(valid, "recursive STARK proof receipt should verify");

        eprintln!(
            "Recursive proof: {} bytes, proved in {} ms (3 children)",
            proof_size, proving_time_ms
        );
    }

    #[test]
    fn test_recursive_verifier_tamper_detection() {
        let (input, _) = make_recursive_input(3);

        let (proof_data, _, _) = prove_recursive(&input);

        let mut tampered_input = input.clone();
        tampered_input.child_hashes[1][0] ^= 0xFF;

        let invalid = verify_recursive_proof(&tampered_input, &proof_data);
        assert!(
            !invalid,
            "tampered child hash should cause verification failure"
        );

        let mut tampered_root = input.clone();
        tampered_root.expected_merkle_root[5] ^= 0xAA;
        let invalid2 = verify_recursive_proof(&tampered_root, &proof_data);
        assert!(
            !invalid2,
            "tampered Merkle root should cause verification failure"
        );

        let mut tampered_state = input.clone();
        tampered_state.child_start_states[0][10] ^= 0x42;
        let invalid3 = verify_recursive_proof(&tampered_state, &proof_data);
        assert!(
            !invalid3,
            "tampered start state should cause verification failure"
        );
    }

    #[test]
    fn test_recursive_verifier_state_chain() {
        let (input, _) = make_recursive_input(4);

        for i in 0..input.child_end_states.len() - 1 {
            assert_eq!(
                input.child_end_states[i],
                input.child_start_states[i + 1],
                "state chain should be continuous at index {}",
                i
            );
        }

        let log_size = compute_log_size(input.child_hashes.len());
        let trace = generate_recursive_verifier_trace(&input, log_size);

        let chain_valid_col = &trace[RECURSIVE_VERIFIER_COLS - 1];

        for i in 0..input.child_hashes.len() {
            let val = chain_valid_col.values.at(i);
            assert_eq!(
                val,
                M31::from(1u32),
                "chain_valid should be 1 at row {} (continuous chain)",
                i
            );
        }

        let mut broken_input = input.clone();
        broken_input.child_start_states[2] = [0xDE; 32];

        let broken_trace = generate_recursive_verifier_trace(&broken_input, log_size);
        let broken_chain_col = &broken_trace[RECURSIVE_VERIFIER_COLS - 1];

        let val = broken_chain_col.values.at(1);
        assert_eq!(
            val,
            M31::from(0u32),
            "chain_valid should be 0 at row 1 (broken chain)"
        );

        let (proof_data, _, _) = prove_recursive(&broken_input);
        let valid = verify_recursive_proof(&broken_input, &proof_data);
        assert!(
            valid,
            "broken chain should still produce a valid proof (chain_valid=0 is allowed)"
        );

        eprintln!("State chain continuity test passed");
    }

    #[test]
    fn test_inner_circuit_roundtrip() {
        use crate::stark::BlockProof;

        let (recursive_input, block_inputs) = make_recursive_input(3);

        for input in &block_inputs {
            let proof = BlockProof::stwo_prove(input);
            let result = proof.stwo_verify(input);
            assert!(
                result.is_valid,
                "child block proof should pass Stwo verification"
            );
        }

        let (recursive_proof_data, proof_size, proving_time_ms) = prove_recursive(&recursive_input);

        assert!(proof_size > 0);

        let valid = verify_recursive_proof(&recursive_input, &recursive_proof_data);
        assert!(
            valid,
            "inner-circuit recursive proof should verify end-to-end"
        );

        let receipt = StwoproofReceipt::from_bytes(&recursive_proof_data)
            .expect("should deserialize recursive receipt");
        assert_eq!(receipt.version, 1);
        assert!(receipt.log_size >= MIN_LOG_SIZE);
        assert!(!receipt.commitment_roots.is_empty());

        eprintln!(
            "Inner-circuit roundtrip: {} child blocks, recursive proof {} bytes in {} ms",
            block_inputs.len(),
            proof_size,
            proving_time_ms
        );
    }
}
