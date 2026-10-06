//! Operator known-answer self-test: every GPU kernel against the CPU engine's
//! operator ([`crate::modern::arith`]) on deterministic pseudo-random inputs
//! that mix model-like magnitudes, wide values, exact edges (0, +-1, +-2^62,
//! i64 extremes, the projection precondition boundary) and inputs outside the
//! profile's domain. Each case passes only when both sides return the same
//! integers or both refuse.
//!
//! The Proof Kit's GPU mode runs a few rounds before the model (a driver that
//! miscompiles an integer operation fails here, by name); CI runs many.

use arc_gpu::modern::{AttentionCase, DyadicRef, GpuModernError, OpLab};

use crate::modern::ModernError;
use crate::modern::arith::{self, DyadicMatrix, HeadCache};
use crate::modern::tables::{EXP_TABLE, attention_lambda};

/// Outcome of a self-test run.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct KatReport {
    /// Cases compared.
    pub cases: usize,
    /// Cases where both sides refused (out of the profile's domain).
    pub refusals: usize,
    /// One line per disagreeing case (at most [`MAX_REPORTED`]).
    pub mismatches: Vec<String>,
    /// Disagreeing cases in total.
    pub mismatch_count: usize,
}

/// Mismatch descriptions kept in a report.
pub const MAX_REPORTED: usize = 20;

impl KatReport {
    pub fn passed(&self) -> bool {
        self.mismatch_count == 0
    }
}

/// SplitMix64: deterministic and identical on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below((hi - lo + 1) as u64) as usize
    }

    /// Uniform magnitude below `2^bits` (bits <= 63), random sign.
    fn signed(&mut self, bits: u32) -> i64 {
        let magnitude = match bits {
            0 => 0,
            63.. => self.next() >> 1,
            _ => self.next() & ((1u64 << bits) - 1),
        };
        let value = magnitude as i64;
        if self.next() & 1 == 0 { value } else { -value }
    }

    /// A value of random width up to `max_bits`, or an exact edge.
    fn value(&mut self, max_bits: u32) -> i64 {
        match self.below(24) {
            0 => 0,
            1 => 1,
            2 => -1,
            3 => 1 << 62,
            4 => -(1 << 62),
            5 => (1 << 62) + 1,
            6 => i64::MAX,
            7 => i64::MIN,
            _ => {
                let bits = self.below(u64::from(max_bits) + 1) as u32;
                self.signed(bits)
            }
        }
    }

    /// Mostly model-like magnitudes (Q16 activations up to ~2^31), sometimes
    /// any width.
    fn activation(&mut self) -> i64 {
        if self.below(8) == 0 {
            self.value(63)
        } else {
            let bits = self.below(32) as u32;
            self.signed(bits)
        }
    }

    fn i32_value(&mut self, wide: bool) -> i32 {
        if wide {
            match self.below(8) {
                0 => i32::MIN,
                1 => i32::MAX,
                _ => self.next() as i32,
            }
        } else {
            let bits = self.below(20) as u32;
            self.signed(bits) as i32
        }
    }
}

fn first_difference(a: &[i64], b: &[i64]) -> String {
    if a.len() != b.len() {
        return format!("lengths {} vs {}", a.len(), b.len());
    }
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(i) => format!("index {i}: CPU {} vs GPU {}", a[i], b[i]),
        None => "equal".into(),
    }
}

fn compare(
    report: &mut KatReport,
    label: &str,
    cpu: Result<Vec<i64>, ModernError>,
    gpu: Result<Vec<i64>, GpuModernError>,
) {
    report.cases += 1;
    let mismatch = match (&cpu, &gpu) {
        (Ok(a), Ok(b)) if a == b => None,
        (Ok(a), Ok(b)) => Some(first_difference(a, b)),
        (Err(ModernError::Domain(_)), Err(GpuModernError::Domain(_))) => {
            report.refusals += 1;
            None
        }
        (Err(c), Ok(_)) => Some(format!("CPU refused ({c}), GPU returned values")),
        (Ok(_), Err(g)) => Some(format!("CPU returned values, GPU failed ({g})")),
        (Err(c), Err(g)) => Some(format!("CPU {c} vs GPU {g}")),
    };
    if let Some(detail) = mismatch {
        report.mismatch_count += 1;
        if report.mismatches.len() < MAX_REPORTED {
            report.mismatches.push(format!("{label}: {detail}"));
        }
    }
}

fn matrix(rng: &mut Rng, rows: usize, cols: usize) -> DyadicMatrix {
    let mut q = Vec::with_capacity(rows * cols);
    let mut mu = Vec::with_capacity(rows);
    let mut k = Vec::with_capacity(rows);
    for _ in 0..rows {
        if rng.below(10) == 0 {
            q.extend(std::iter::repeat_n(0i8, cols));
            mu.push(0);
            k.push(16);
            continue;
        }
        let small = rng.below(3) == 0;
        for _ in 0..cols {
            let w = if small {
                rng.range(0, 6) as i64 - 3
            } else {
                rng.range(0, 254) as i64 - 127
            };
            q.push(w as i8);
        }
        mu.push(((1u64 << 30) + rng.below(1 << 30)) as i32);
        k.push(match rng.below(6) {
            0 => 16,
            1 => 62,
            _ => 30 + rng.below(20) as u8,
        });
    }
    DyadicMatrix {
        rows,
        cols,
        q,
        mu,
        k,
    }
}

fn dyadic(m: &DyadicMatrix) -> DyadicRef<'_> {
    DyadicRef {
        rows: m.rows,
        cols: m.cols,
        q: &m.q,
        mu: &m.mu,
        k: &m.k,
    }
}

fn project_case(lab: &OpLab, rng: &mut Rng, round: usize, report: &mut KatReport) {
    let rows = rng.range(1, 24);
    let cols = rng.range(1, 70);
    let m = matrix(rng, rows, cols);
    let x: Vec<i64> = match rng.below(4) {
        // At the projection precondition: 127 * sum |x| just below or above 2^63.
        0 => {
            let mut x = vec![0i64; cols];
            let bound = (i64::MAX / 127) as u64; // (2^63 - 1) / 127
            let total = bound - 1 + rng.below(3); // bound - 1, bound (accepted), bound + 1 (refused)
            x[0] = (total / 2) as i64;
            x[cols - 1] += (total - total / 2) as i64;
            x
        }
        1 => (0..cols).map(|_| rng.value(63)).collect(),
        _ => (0..cols).map(|_| rng.activation()).collect(),
    };
    let mut out = vec![0i64; rows];
    let cpu = arith::project(&m, &x, &mut out).map(|()| out);
    let gpu = lab.project(&dyadic(&m), &x);
    compare(
        report,
        &format!("project #{round} ({rows}x{cols})"),
        cpu,
        gpu,
    );
}

fn rms_case(lab: &OpLab, rng: &mut Rng, round: usize, report: &mut KatReport) {
    let n = if rng.below(4) == 0 {
        rng.range(257, 600)
    } else {
        rng.range(1, 64)
    };
    let x: Vec<i64> = (0..n).map(|_| rng.activation()).collect();
    let gain: Vec<i64> = (0..n)
        .map(|_| match rng.below(8) {
            0 => rng.value(63),
            1 => -(65_536 + rng.signed(14)),
            _ => 65_536 + rng.signed(15),
        })
        .collect();
    let eps = match rng.below(3) {
        0 => 1,
        1 => 4295,
        _ => 1 + rng.below(1 << 40) as i64,
    };
    let cpu = arith::rms_norm(&x, &gain, eps);
    let gpu = lab.rms_norm(&x, &gain, eps);
    compare(
        report,
        &format!("rms_norm #{round} (n {n}, eps {eps})"),
        cpu,
        gpu,
    );
}

fn rope_case(lab: &OpLab, rng: &mut Rng, round: usize, report: &mut KatReport) {
    let d_head = [2usize, 4, 8, 16][rng.below(4) as usize];
    let heads = rng.range(1, 3);
    let half = d_head / 2;
    let data: Vec<i64> = (0..heads * d_head).map(|_| rng.activation()).collect();
    let wide = rng.below(4) == 0;
    let table = |rng: &mut Rng| -> Vec<i32> {
        (0..half)
            .map(|_| {
                if wide {
                    rng.i32_value(true)
                } else {
                    rng.range(0, 131_074) as i32 - 65_537
                }
            })
            .collect()
    };
    let cos = table(rng);
    let sin = table(rng);
    let mut cpu_data = data.clone();
    let cpu = cpu_data
        .chunks_exact_mut(d_head)
        .try_for_each(|head| arith::rope_split_half(head, &cos, &sin))
        .map(|()| cpu_data);
    let gpu = lab.rope(&data, d_head, &cos, &sin);
    compare(
        report,
        &format!("rope #{round} ({heads}x{d_head})"),
        cpu,
        gpu,
    );
}

fn attention_case(lab: &OpLab, rng: &mut Rng, round: usize, report: &mut KatReport) {
    let d_head = [2usize, 4, 8][rng.below(3) as usize];
    let n_kv_heads = rng.range(1, 2);
    let n_heads = n_kv_heads * rng.range(1, 3);
    let positions = if rng.below(5) == 0 {
        rng.range(129, 300)
    } else {
        rng.range(1, 40)
    };
    let stride = n_kv_heads * d_head;
    let wide = rng.below(5) == 0;
    let q: Vec<i64> = (0..n_heads * d_head)
        .map(|_| {
            if rng.below(12) == 0 {
                rng.value(63)
            } else {
                let bits = rng.below(24) as u32;
                rng.signed(bits)
            }
        })
        .collect();
    let keys: Vec<i32> = (0..positions * stride)
        .map(|_| rng.i32_value(wide))
        .collect();
    let values: Vec<i32> = (0..positions * stride)
        .map(|_| rng.i32_value(wide))
        .collect();
    let lambda = if rng.below(4) == 0 {
        1 + rng.below(1 << 30) as i64
    } else {
        attention_lambda(d_head)
    };
    let group = n_heads / n_kv_heads;
    let mut out = vec![0i64; n_heads * d_head];
    let cpu = out
        .chunks_mut(d_head)
        .zip(q.chunks(d_head))
        .enumerate()
        .try_for_each(|(head, (o, q_head))| {
            let view = HeadCache {
                keys: &keys,
                values: &values,
                positions,
                stride,
                offset: (head / group) * d_head,
            };
            arith::attention_head(q_head, view, lambda, o)
        })
        .map(|()| out);
    let case = AttentionCase {
        q: &q,
        keys: &keys,
        values: &values,
        positions,
        n_heads,
        n_kv_heads,
        d_head,
        lambda,
    };
    let gpu = lab.attention(&case, &EXP_TABLE);
    compare(
        report,
        &format!(
            "attention #{round} ({n_heads}/{n_kv_heads} heads x {d_head}, {positions} positions)"
        ),
        cpu,
        gpu,
    );
}

fn silu_case(lab: &OpLab, rng: &mut Rng, round: usize, report: &mut KatReport) {
    let n = rng.range(1, 200);
    let gate: Vec<i64> = (0..n).map(|_| rng.activation()).collect();
    let up: Vec<i64> = (0..n).map(|_| rng.activation()).collect();
    let cpu = gate
        .iter()
        .zip(&up)
        .map(|(&g, &u)| arith::gated_silu(g, u))
        .collect::<Result<Vec<i64>, _>>();
    let gpu = lab.gated_silu(&gate, &up, &EXP_TABLE);
    compare(report, &format!("gated_silu #{round} (n {n})"), cpu, gpu);
}

fn residual_case(lab: &OpLab, rng: &mut Rng, round: usize, report: &mut KatReport) {
    let n = rng.range(1, 200);
    let edge = rng.below(3) == 0;
    let draw = |rng: &mut Rng| {
        if edge {
            rng.value(63)
        } else {
            rng.activation()
        }
    };
    let h: Vec<i64> = (0..n).map(|_| draw(rng)).collect();
    let delta: Vec<i64> = (0..n).map(|_| draw(rng)).collect();
    let mut cpu_h = h.clone();
    let cpu = arith::add_residual(&mut cpu_h, &delta).map(|()| cpu_h);
    let gpu = lab.residual(&h, &delta);
    compare(report, &format!("residual #{round} (n {n})"), cpu, gpu);
}

fn embed_case(lab: &OpLab, rng: &mut Rng, round: usize, report: &mut KatReport) {
    let rows = rng.range(1, 60);
    let cols = rng.range(1, 40);
    let m = matrix(rng, rows, cols);
    let count = rng.range(1, 4);
    let tokens: Vec<u32> = (0..count).map(|_| rng.below(rows as u64) as u32).collect();
    let cpu = tokens
        .iter()
        .map(|&t| arith::embed_row(&m, t as usize))
        .collect::<Result<Vec<Vec<i64>>, _>>()
        .map(|parts| parts.concat());
    let gpu = lab.embed(&dyadic(&m), &tokens);
    compare(
        report,
        &format!("embed #{round} ({rows}x{cols}, {count} tokens)"),
        cpu,
        gpu,
    );
}

/// Run `rounds` rounds of every operator from `seed`.
pub fn run(lab: &OpLab, seed: u64, rounds: usize) -> KatReport {
    let mut rng = Rng(seed);
    let mut report = KatReport::default();
    for round in 0..rounds {
        project_case(lab, &mut rng, round, &mut report);
        rms_case(lab, &mut rng, round, &mut report);
        rope_case(lab, &mut rng, round, &mut report);
        attention_case(lab, &mut rng, round, &mut report);
        silu_case(lab, &mut rng, round, &mut report);
        residual_case(lab, &mut rng, round, &mut report);
        embed_case(lab, &mut rng, round, &mut report);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_are_deterministic_and_cover_edges() {
        let mut a = Rng(7);
        let mut b = Rng(7);
        let xs: Vec<i64> = (0..2000).map(|_| a.value(63)).collect();
        let ys: Vec<i64> = (0..2000).map(|_| b.value(63)).collect();
        assert_eq!(xs, ys);
        for edge in [0, 1, -1, 1 << 62, -(1 << 62), i64::MAX, i64::MIN] {
            assert!(xs.contains(&edge), "{edge} never drawn");
        }
        let m = matrix(&mut a, 30, 9);
        assert!(dyadic(&m).q.iter().all(|&w| w != i8::MIN));
        assert!(m.validate("kat").is_ok());
    }

    #[test]
    fn gpu_operators_match_cpu_operators() {
        let lab = match OpLab::new(std::env::var("ARC_GPU_ADAPTER").ok().as_deref()) {
            Ok(lab) => lab,
            Err(error) => {
                let message = error.to_string();
                let required = std::env::var("ARC_GPU_REQUIRE").as_deref() == Ok("1");
                assert!(
                    !required && message.contains("no usable GPU adapter"),
                    "GPU unavailable: {message}"
                );
                eprintln!("SKIP (no GPU adapter): {message}");
                return;
            }
        };
        let rounds = std::env::var("ARC_GPU_KAT_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40);
        let report = run(&lab, 0x00A2_C0DE, rounds);
        eprintln!(
            "GPU operator KAT on {} ({}): {} cases, {} refused on both sides, {} mismatches",
            lab.report().name,
            lab.report().backend,
            report.cases,
            report.refusals,
            report.mismatch_count
        );
        assert!(report.passed(), "{:#?}", report.mismatches);
        assert!(
            report.refusals > 0,
            "the generators never reached the domain edges"
        );
    }
}
