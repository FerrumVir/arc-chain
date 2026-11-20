use arc_inference::cached_integer_model::{I8Weights, I16Weights};

const ONE: i64 = 1 << 16;

fn matmul_i16_scalar(weights: &I16Weights, input: &[i64]) -> Vec<i64> {
    let n_rows = weights.n_rows;
    let n_cols = weights.n_cols;
    assert_eq!(input.len(), n_cols);
    let mut out = vec![0i64; n_rows];
    for (i, o) in out.iter_mut().enumerate() {
        let mut acc: i128 = 0;
        for (j, &x) in input[..n_cols].iter().enumerate() {
            acc += (weights.data[i * n_cols + j] as i128) * (x as i128);
        }
        let wide = acc * (weights.scales[i] as i128);
        *o = ((wide / 32767) >> 16) as i64;
    }
    out
}

fn matmul_i8_scalar(weights: &I8Weights, input: &[i64]) -> Vec<i64> {
    let n_rows = weights.n_rows;
    let n_cols = weights.n_cols;
    let mut out = vec![0i64; n_rows];
    for (i, o) in out.iter_mut().enumerate() {
        let mut acc: i64 = 0;
        for (j, &x) in input[..n_cols].iter().enumerate() {
            acc += (weights.data[i * n_cols + j] as i64) * x;
        }
        *o = (acc * weights.scales[i]) >> 16;
    }
    out
}

fn main() {
    let f32_weights: Vec<f32> = vec![1.0, -2.0, 0.5, 3.0, -1.5, 2.5, -0.8, 1.2];
    let f32_input: Vec<f32> = vec![1.2, -0.8, 0.5, 2.1, -1.0, 0.7, 1.5, -0.3];

    let truth: f32 = f32_weights
        .iter()
        .zip(f32_input.iter())
        .map(|(w, x)| w * x)
        .sum();
    println!("Ground truth dot: {:.6}", truth);
    println!("Expected Q16:    {}", (truth * ONE as f32).round() as i64);
    println!();

    let input_q16: Vec<i64> = f32_input
        .iter()
        .map(|&x| (x * ONE as f32).round() as i64)
        .collect();
    println!("Input Q16: {:?}", input_q16);
    println!();

    let w_a = I16Weights::quantize_f32(&f32_weights, 1, 8);
    println!("I16::quantize_f32:");
    println!("  data   = {:?}", w_a.data);
    println!("  scales = {:?}", w_a.scales);
    let out_a = matmul_i16_scalar(&w_a, &input_q16);
    println!(
        "  output = {} (real = {:.6})",
        out_a[0],
        out_a[0] as f32 / ONE as f32
    );
    println!();

    let w_i8 = I8Weights::quantize_f32(&f32_weights, 1, 8);
    let w_b = I16Weights::from_i8(&w_i8);
    println!("I8::quantize_f32 + I16::from_i8:");
    println!("  i8 data   = {:?}", w_i8.data);
    println!("  i8 scales = {:?}", w_i8.scales);
    println!("  i16 data  = {:?}", w_b.data);
    println!("  i16 scales= {:?}", w_b.scales);
    let out_b = matmul_i16_scalar(&w_b, &input_q16);
    println!(
        "  output = {} (real = {:.6})",
        out_b[0],
        out_b[0] as f32 / ONE as f32
    );
    println!();

    let out_c = matmul_i8_scalar(&w_i8, &input_q16);
    println!("I8 matmul baseline (works in production):");
    println!(
        "  output = {} (real = {:.6})",
        out_c[0],
        out_c[0] as f32 / ONE as f32
    );
    println!();

    println!("=== Comparison ===");
    let truth_q16 = (truth * ONE as f32).round() as i64;
    println!("truth_q16:           {}", truth_q16);
    println!(
        "I16 quantize_f32:    {} (ratio to truth: {:.3})",
        out_a[0],
        out_a[0] as f64 / truth_q16 as f64
    );
    println!(
        "I16 from_i8:         {} (ratio to truth: {:.3})",
        out_b[0],
        out_b[0] as f64 / truth_q16 as f64
    );
    println!(
        "I8 baseline:         {} (ratio to truth: {:.3})",
        out_c[0],
        out_c[0] as f64 / truth_q16 as f64
    );
}
