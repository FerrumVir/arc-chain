// Side-by-side candle vs integer forward at MULTIPLE positions. If our forward
// matches candle at position 0 but diverges at position k, the KV cache path
// (storage, attention read, online softmax, or RoPE position handling) is the
// bug site.
use arc_inference::cached_integer_model::{load_cached_model, KVCache};
use candle_core::{Device, Tensor};
use candle_transformers::models::quantized_llama::ModelWeights;

const MODEL_PATH: &str = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";

fn argmax_f32(v: &[f32]) -> (usize, f32) {
    let mut ai = 0;
    let mut av = f32::NEG_INFINITY;
    for (i, &x) in v.iter().enumerate() { if x > av { av = x; ai = i; } }
    (ai, av)
}
fn argmax_i64(v: &[i64]) -> (usize, i64) {
    let mut ai = 0;
    let mut av = i64::MIN;
    for (i, &x) in v.iter().enumerate() { if x > av { av = x; ai = i; } }
    (ai, av)
}

fn topk_f32(v: &[f32], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&a, &b| v[b].partial_cmp(&v[a]).unwrap());
    idx.truncate(k); idx
}
fn topk_i64(v: &[i64], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&a, &b| v[b].cmp(&v[a]));
    idx.truncate(k); idx
}

fn main() {
    let device = Device::Cpu;

    eprintln!("Loading candle...");
    let mut file = std::fs::File::open(MODEL_PATH).unwrap();
    let content = candle_core::quantized::gguf_file::Content::read(&mut file).unwrap();
    let mut candle_model = ModelWeights::from_gguf(content, &mut file, &device).unwrap();

    eprintln!("Loading integer...");
    let integer_model = load_cached_model(MODEL_PATH).unwrap();
    let mut cache = KVCache::new(integer_model.config.n_layers);

    // Prompt: "The capital of France is"
    // Real-tokenized via Llama BPE: [1, 450, 7483, 310, 3444, 338]
    // (1=BOS, 450="The", 7483=" capital", 310=" of", 3444=" France", 338=" is")
    let tokens: Vec<u32> = vec![1, 450, 7483, 310, 3444, 338];

    eprintln!("\n{:^3}  {:^6}  {:^5}  {:>10}  {:>10}  {:^5}  top10-overlap", "pos", "tok_in", "c_amx", "c_logit", "our_logit", "o_amx");
    eprintln!("{}", "-".repeat(80));

    for (pos, &tok) in tokens.iter().enumerate() {
        // Candle: feed just this token, letting candle manage its own KV cache
        let t = Tensor::new(&[tok], &device).unwrap().unsqueeze(0).unwrap();
        let cl = candle_model.forward(&t, pos).unwrap();
        let cl = cl.squeeze(0).unwrap();
        let cl = if cl.dims().len() == 2 {
            cl.get(cl.dim(0).unwrap() - 1).unwrap()
        } else { cl };
        let c_vec: Vec<f32> = cl.to_vec1().unwrap();
        let (c_ai, c_av) = argmax_f32(&c_vec);

        // Ours: feed this token, accumulate KV
        let ol = integer_model.forward_one_token(tok, &mut cache);
        let (o_ai, o_av) = argmax_i64(&ol);

        let ctop = topk_f32(&c_vec, 10);
        let otop100: std::collections::HashSet<usize> = topk_i64(&ol, 100).into_iter().collect();
        let overlap = ctop.iter().filter(|i| otop100.contains(i)).count();

        eprintln!("{:3}  {:>6}  {:>5}  {:>10.3}  {:>10.3}  {:>5}  {}/10",
            pos, tok, c_ai, c_av, o_av as f64 / 65536.0, o_ai, overlap);
    }
}
