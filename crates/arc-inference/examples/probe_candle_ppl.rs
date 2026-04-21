// Run candle through the same PPL loop our eval_perplexity uses.
// If candle's PPL on the same tokens is ~5.5 → tokenization is correct and
// something else in our integer path is off even though per-step argmax matches.
// If candle's PPL is ~155 → the data/text/tokenization is bad (not our bug).
use candle_core::{Device, Tensor};
use candle_transformers::models::quantized_llama::ModelWeights;

const MODEL_PATH: &str = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";
const TOKENS_PATH: &str = "/Users/tjdunham/.arc-models/wiki.test.llama_bpe.json";

fn main() {
    let device = Device::Cpu;

    eprintln!("Loading candle...");
    let mut file = std::fs::File::open(MODEL_PATH).unwrap();
    let content = candle_core::quantized::gguf_file::Content::read(&mut file).unwrap();
    let mut model = ModelWeights::from_gguf(content, &mut file, &device).unwrap();

    let tokens_raw = std::fs::read_to_string(TOKENS_PATH).unwrap();
    let tokens: Vec<u32> = serde_json::from_str::<Vec<u64>>(&tokens_raw).unwrap()
        .into_iter().map(|x| x as u32).collect();
    let n = tokens.len().min(256);
    eprintln!("Evaluating {} tokens on candle reference", n);

    // BOS
    let bos = Tensor::new(&[1u32], &device).unwrap().unsqueeze(0).unwrap();
    let _ = model.forward(&bos, 0).unwrap();

    let mut nll_sum: f64 = 0.0;
    let mut n_eval: usize = 0;

    for i in 0..n - 1 {
        let tok = tokens[i];
        let next_tok = tokens[i + 1] as usize;

        let t = Tensor::new(&[tok], &device).unwrap().unsqueeze(0).unwrap();
        let logits = model.forward(&t, i + 1).unwrap();
        let logits = logits.squeeze(0).unwrap();
        let logits = if logits.dims().len() == 2 {
            logits.get(logits.dim(0).unwrap() - 1).unwrap()
        } else { logits };
        let v: Vec<f32> = logits.to_vec1().unwrap();

        // log-softmax
        let max = v.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let log_sum_exp: f64 = v.iter().map(|&x| ((x - max) as f64).exp()).sum::<f64>().ln() + max as f64;
        let log_p = if next_tok < v.len() {
            v[next_tok] as f64 - log_sum_exp
        } else {
            -(v.len() as f64).ln()
        };
        nll_sum -= log_p;
        n_eval += 1;

        if (i + 1) % 50 == 0 || i == n - 2 {
            let ppl = (nll_sum / n_eval as f64).exp();
            eprintln!("[{}/{}] candle PPL: {:.2}", i + 1, n - 1, ppl);
        }
    }
    let final_ppl = (nll_sum / n_eval as f64).exp();
    eprintln!("\nFINAL candle PPL on {} tokens: {:.2}", n_eval, final_ppl);
}
