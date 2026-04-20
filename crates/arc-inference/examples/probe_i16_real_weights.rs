// Load ONE real Llama-2-7B weight tensor from GGUF, quantize three ways,
// matmul against a realistic Q16 input, and compare to f32 ground truth.
// Goal: localize whether the I16 regression is in the per-row math (probe
// should fail) or higher up the stack (probe passes, bug is elsewhere).
use arc_inference::cached_integer_model::{I8Weights, I16Weights};
use candle_core::{Device, quantized::gguf_file};

const ONE: i64 = 1 << 16;
const MODEL_PATH: &str = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";

fn extract_f32(name: &str) -> Vec<f32> {
    let device = Device::Cpu;
    let mut reader = std::fs::File::open(MODEL_PATH).expect("open");
    let content = gguf_file::Content::read(&mut reader).expect("gguf");
    let qt = content.tensor(&mut reader, name, &device).expect(name);
    let deq = qt.dequantize(&device).expect("dequant");
    deq.flatten_all().expect("flat").to_vec1::<f32>().expect("to_vec1")
}

fn matmul_f32(w: &[f32], rows: usize, cols: usize, input: &[f32]) -> Vec<f32> {
    assert_eq!(input.len(), cols);
    (0..rows).map(|i| {
        let mut acc = 0.0f64;
        for j in 0..cols {
            acc += (w[i * cols + j] as f64) * (input[j] as f64);
        }
        acc as f32
    }).collect()
}

fn matmul_i8_scalar(w: &I8Weights, input: &[i64]) -> Vec<i64> {
    (0..w.n_rows).map(|i| {
        let mut acc: i64 = 0;
        for j in 0..w.n_cols {
            acc += (w.data[i * w.n_cols + j] as i64) * input[j];
        }
        (acc * w.scales[i]) >> 16
    }).collect()
}

fn matmul_i16_scalar(w: &I16Weights, input: &[i64]) -> Vec<i64> {
    (0..w.n_rows).map(|i| {
        let mut acc: i128 = 0;
        for j in 0..w.n_cols {
            acc += (w.data[i * w.n_cols + j] as i128) * (input[j] as i128);
        }
        let wide = acc * (w.scales[i] as i128);
        ((wide / 32767) >> 16) as i64
    }).collect()
}

fn stats(label: &str, v: &[f64]) {
    let abs: Vec<f64> = v.iter().map(|x| x.abs()).collect();
    let max = abs.iter().cloned().fold(0.0f64, f64::max);
    let mean = abs.iter().sum::<f64>() / abs.len() as f64;
    let mut sorted = abs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[sorted.len() / 2];
    let p99 = sorted[sorted.len() * 99 / 100];
    println!("{:50} max={:.4e} mean={:.4e} p50={:.4e} p99={:.4e}",
        label, max, mean, p50, p99);
}

fn compare(label: &str, actual: &[i64], truth: &[f32]) {
    assert_eq!(actual.len(), truth.len());
    let diffs: Vec<f64> = actual.iter().zip(truth.iter())
        .map(|(a, t)| {
            let a_real = *a as f64 / ONE as f64;
            a_real - *t as f64
        }).collect();
    let rel_diffs: Vec<f64> = actual.iter().zip(truth.iter())
        .filter(|(_, t)| t.abs() > 0.01)
        .map(|(a, t)| {
            let a_real = *a as f64 / ONE as f64;
            (a_real - *t as f64) / (*t as f64).abs()
        }).collect();
    let ratios: Vec<f64> = actual.iter().zip(truth.iter())
        .filter(|(_, t)| t.abs() > 0.01)
        .map(|(a, t)| (*a as f64 / ONE as f64) / *t as f64)
        .collect();

    let mean_ratio = ratios.iter().sum::<f64>() / ratios.len() as f64;
    let mut sorted_ratios = ratios.clone();
    sorted_ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());

    println!("  {} vs f32 truth:", label);
    stats("    absolute diff", &diffs);
    stats("    relative diff (|truth|>0.01)", &rel_diffs);
    println!("    ratio (actual/truth) mean={:.4} min={:.4} max={:.4}",
        mean_ratio, sorted_ratios[0], sorted_ratios[sorted_ratios.len() - 1]);
}

fn probe(tensor_name: &str, rows: usize, cols: usize) {
    println!("\n=== {} [{}×{}] ===", tensor_name, rows, cols);
    let f32w = extract_f32(tensor_name);
    assert_eq!(f32w.len(), rows * cols, "shape mismatch");

    // Stats on the raw weights
    let abs_max_per_row: Vec<f32> = (0..rows).map(|i| {
        f32w[i * cols..(i + 1) * cols].iter().map(|x| x.abs()).fold(0.0f32, f32::max)
    }).collect();
    let small_rows = abs_max_per_row.iter().filter(|&&x| x < 1.0).count();
    println!("Rows with abs_max < 1.0: {} / {}", small_rows, rows);
    let min_am = abs_max_per_row.iter().cloned().fold(f32::INFINITY, f32::min);
    let max_am = abs_max_per_row.iter().cloned().fold(0.0f32, f32::max);
    println!("abs_max range: [{:.4}, {:.4}]", min_am, max_am);

    // Realistic Q16 input: RMSNormed activations, roughly N(0, 1) scaled to Q16
    let mut seed: u64 = 42;
    let f32_input: Vec<f32> = (0..cols).map(|_| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        // Box-Muller-ish trick: two uniforms → approximately normal via sum
        let u1 = ((seed >> 33) as f64 / u32::MAX as f64) - 0.5;
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u2 = ((seed >> 33) as f64 / u32::MAX as f64) - 0.5;
        ((u1 + u2) * 2.0) as f32   // approximately N(0, ~0.67)
    }).collect();
    let input_q16: Vec<i64> = f32_input.iter()
        .map(|&x| (x as f64 * ONE as f64).round() as i64).collect();

    // Ground truth
    let truth = matmul_f32(&f32w, rows, cols, &f32_input);

    // Paths
    let w_i8 = I8Weights::quantize_f32(&f32w, rows, cols);
    let out_i8 = matmul_i8_scalar(&w_i8, &input_q16);

    let w_i16_f32 = I16Weights::quantize_f32(&f32w, rows, cols);
    let out_i16_f32 = matmul_i16_scalar(&w_i16_f32, &input_q16);

    let w_i16_from_i8 = I16Weights::from_i8(&w_i8);
    let out_i16_from_i8 = matmul_i16_scalar(&w_i16_from_i8, &input_q16);

    compare("I8::quantize_f32 → matmul_i8", &out_i8, &truth);
    compare("I16::quantize_f32 → matmul_i16", &out_i16_f32, &truth);
    compare("I16::from_i8 → matmul_i16", &out_i16_from_i8, &truth);
}

fn main() {
    probe("blk.0.attn_q.weight", 4096, 4096);
    probe("blk.0.ffn_down.weight", 4096, 11008);
    probe("output.weight", 32000, 4096);
}
