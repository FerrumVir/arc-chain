// Verify block-i8 is actually dispatching during forward_one_token.
// We compare logits from the full-forward with block-i8 enabled vs disabled;
// if they differ, dispatch is working. If they match, something is bypassing
// the block-i8 path.
use arc_inference::cached_integer_model::{load_cached_model, KVCache};

fn main() {
    let path = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";
    eprintln!("Loading...");
    let mut model = load_cached_model(path).expect("load");
    eprintln!("block_i8_layers: {}", model.block_i8_layers.as_ref().map(|v| v.len()).unwrap_or(0));
    eprintln!("block_i8_output: {}", model.block_i8_output.is_some());

    // With block-i8 enabled
    let mut cache_a = KVCache::new(model.config.n_layers);
    let _ = model.forward_one_token(1, &mut cache_a);
    let logits_with = model.forward_one_token(5000, &mut cache_a);
    let amax_w = logits_with.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
    eprintln!("WITH block-i8:  argmax={} val={} logits[0..4]={:?}",
        amax_w.0, amax_w.1, &logits_with[..4]);

    // Disable block-i8
    let saved_l = model.block_i8_layers.take();
    let saved_o = model.block_i8_output.take();

    let mut cache_b = KVCache::new(model.config.n_layers);
    let _ = model.forward_one_token(1, &mut cache_b);
    let logits_without = model.forward_one_token(5000, &mut cache_b);
    let amax_wo = logits_without.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
    eprintln!("WITHOUT block-i8: argmax={} val={} logits[0..4]={:?}",
        amax_wo.0, amax_wo.1, &logits_without[..4]);

    // Restore
    model.block_i8_layers = saved_l;
    model.block_i8_output = saved_o;

    // Compare
    let identical = logits_with == logits_without;
    eprintln!("Identical: {}", identical);
    if identical {
        eprintln!("⚠ block-i8 is NOT dispatching - forward is using I8 fallback.");
    } else {
        let max_abs_diff = logits_with.iter().zip(logits_without.iter())
            .map(|(a, b)| (a - b).abs()).max().unwrap();
        let ratio_with: f64 = logits_with.iter().map(|v| v.abs()).sum::<i64>() as f64
            / logits_without.iter().map(|v| v.abs()).sum::<i64>() as f64;
        eprintln!("max_abs_diff = {}", max_abs_diff);
        eprintln!("mean|logit| ratio with/without = {:.3}", ratio_with);
    }
}
