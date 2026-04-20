// A/B probe: run same forward pass with different I16 dispatch combinations
// to localize which matmul is producing the ~94× magnitude bug.
use arc_inference::cached_integer_model::{load_cached_model, KVCache, CachedIntegerModel};

fn forward_token(model: &CachedIntegerModel, tok: u32) -> Vec<i64> {
    let mut cache = KVCache::new(model.config.n_layers);
    let _ = model.forward_one_token(1, &mut cache);
    model.forward_one_token(tok, &mut cache)
}

fn summarize(label: &str, logits: &[i64]) {
    let argmax = logits.iter().enumerate().max_by_key(|(_, v)| **v).map(|(i, _)| i).unwrap();
    let max_abs = logits.iter().map(|v| v.abs()).max().unwrap_or(0);
    let mean_abs: i64 = logits.iter().map(|v| v.abs()).sum::<i64>() / logits.len() as i64;
    eprintln!("{:35} argmax={:5} max_abs={:8} mean_abs={:6}", label, argmax, max_abs, mean_abs);
}

fn main() {
    let path = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";
    eprintln!("Loading...");
    let mut model = load_cached_model(path).expect("load");
    eprintln!("Loaded.\n");

    summarize("I16 layers + I16 output", &forward_token(&model, 5000));

    let saved_out = model.i16_output.take();
    summarize("I16 layers + I8 output", &forward_token(&model, 5000));
    model.i16_output = saved_out;

    let saved_layers = model.i16_layers.take();
    summarize("I8 layers + I16 output", &forward_token(&model, 5000));
    model.i16_layers = saved_layers;

    let saved_layers = model.i16_layers.take();
    let saved_out = model.i16_output.take();
    summarize("I8 layers + I8 output (baseline)", &forward_token(&model, 5000));
    model.i16_layers = saved_layers;
    model.i16_output = saved_out;
}
