// Real-Llama probe for the new block-wise INT8 path.
// Measures: block_i8 quantize + matmul vs f32 ground truth vs the old
// per-row I8 path, on the actual tensors that produced the 107-PPL ceiling.
use arc_inference::block_i8::{BlockI8Weights, matmul_block_i8, BLOCK_SIZE};
use arc_inference::cached_integer_model::I8Weights;
use candle_core::{Device, quantized::gguf_file};

const ONE: i64 = 1 << 16;
const MODEL_PATH: &str = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";

fn extract_f32(name: &str) -> Vec<f32> {
    let device = Device::Cpu;
    let mut r = std::fs::File::open(MODEL_PATH).expect("open");
    let content = gguf_file::Content::read(&mut r).expect("gguf");
    let qt = content.tensor(&mut r, name, &device).expect(name);
    qt.dequantize(&device).expect("dq").flatten_all().expect("f").to_vec1::<f32>().expect("v")
}

fn matmul_f32(w: &[f32], rows: usize, cols: usize, input: &[f32]) -> Vec<f32> {
    (0..rows).map(|i| {
        let mut a = 0.0f64;
        for j in 0..cols { a += (w[i * cols + j] as f64) * (input[j] as f64); }
        a as f32
    }).collect()
}

fn matmul_i8_scalar(w: &I8Weights, input: &[i64]) -> Vec<i64> {
    (0..w.n_rows).map(|i| {
        let mut a: i64 = 0;
        for j in 0..w.n_cols { a += (w.data[i * w.n_cols + j] as i64) * input[j]; }
        (a * w.scales[i]) >> 16
    }).collect()
}

fn compare(label: &str, actual: &[i64], truth: &[f32]) {
    let ratios: Vec<f64> = actual.iter().zip(truth.iter())
        .filter(|(_, t)| t.abs() > 0.01)
        .map(|(a, t)| (*a as f64 / ONE as f64) / *t as f64)
        .collect();
    let mean_r = ratios.iter().sum::<f64>() / ratios.len().max(1) as f64;
    let min_r = ratios.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_r = ratios.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let rel_errs: Vec<f64> = actual.iter().zip(truth.iter())
        .filter(|(_, t)| t.abs() > 0.01)
        .map(|(a, t)| ((*a as f64 / ONE as f64) - *t as f64).abs() / t.abs() as f64)
        .collect();
    let mut sorted = rel_errs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[sorted.len() / 2];
    let p99 = sorted[sorted.len() * 99 / 100];

    println!("  {:40} mean_ratio={:.4} range=[{:.3},{:.3}] p50_err={:.4} p99_err={:.4}",
        label, mean_r, min_r, max_r, p50, p99);
}

fn probe(name: &str, rows: usize, cols: usize) {
    println!("\n=== {} [{}×{}] ===", name, rows, cols);
    let f = extract_f32(name);
    assert_eq!(f.len(), rows * cols);

    // Realistic Q16 input ~ N(0, 0.7)
    let mut s: u64 = 42;
    let input_f32: Vec<f32> = (0..cols).map(|_| {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u1 = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u2 = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
        ((u1 + u2) * 2.0) as f32
    }).collect();
    let input_q16: Vec<i64> = input_f32.iter()
        .map(|&x| (x as f64 * ONE as f64).round() as i64).collect();

    let truth = matmul_f32(&f, rows, cols, &input_f32);

    let w_i8 = I8Weights::quantize_f32(&f, rows, cols);
    let out_i8 = matmul_i8_scalar(&w_i8, &input_q16);

    let w_b = BlockI8Weights::quantize_f32(&f, rows, cols);
    let out_b = matmul_block_i8(&w_b, &input_q16);

    compare("old per-row I8", &out_i8, &truth);
    compare("new block-wise I8 (BLOCK=32)", &out_b, &truth);

    let bytes_old = rows * cols + rows * 8;
    let bytes_new = w_b.memory_bytes();
    println!("  memory: old={} MB  new={} MB  overhead={:.1}%",
        bytes_old / 1024 / 1024,
        bytes_new / 1024 / 1024,
        100.0 * (bytes_new as f64 / bytes_old as f64 - 1.0));
}

fn main() {
    println!("BLOCK_SIZE = {}", BLOCK_SIZE);
    probe("blk.0.attn_q.weight", 4096, 4096);
    probe("blk.0.ffn_down.weight", 4096, 11008);
    probe("output.weight", 32000, 4096);
}
