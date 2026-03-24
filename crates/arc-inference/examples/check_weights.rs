//! Check raw GGUF weight values to find quantization/loading bugs.

fn main() {
    use candle_core::Device;
    use candle_core::quantized::gguf_file;

    let path = std::env::args().nth(1).unwrap_or("/tmp/tinyllama-1.1b-chat.Q8_0.gguf".into());
    let mut reader = std::fs::File::open(&path).unwrap();
    let content = gguf_file::Content::read(&mut reader).unwrap();

    // Print first 8 values of key tensors
    let device = Device::Cpu;
    let mut check = |name: &str| {
        match content.tensor(&mut reader, name, &device) {
            Ok(qt) => {
                let dims = qt.shape().dims();
                match qt.dequantize(&device) {
                    Ok(t) => {
                        let flat = t.flatten_all().unwrap();
                        let vals: Vec<f32> = flat.to_vec1().unwrap();
                        let first8: Vec<f32> = vals[..8.min(vals.len())].to_vec();
                        let min = vals.iter().cloned().fold(f32::INFINITY, f32::min);
                        let max = vals.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                        let mean: f32 = vals.iter().sum::<f32>() / vals.len() as f32;
                        println!("{:30} {:?} first8={:?}", name, dims, first8);
                        println!("{:30} range=[{:.4}, {:.4}] mean={:.6}", "", min, max, mean);
                    }
                    Err(e) => println!("{:30} dequant error: {}", name, e),
                }
            }
            Err(e) => println!("{:30} not found: {}", name, e),
        }
    };

    println!("=== GGUF Weight Check ===\n");
    check("token_embd.weight");
    check("output.weight");
    check("output_norm.weight");
    println!();
    check("blk.0.attn_norm.weight");
    check("blk.0.ffn_norm.weight");
    check("blk.0.attn_q.weight");
    check("blk.0.attn_k.weight");
    check("blk.0.ffn_gate.weight");
    println!();
    check("blk.10.attn_norm.weight");
    check("blk.21.attn_norm.weight");
}
