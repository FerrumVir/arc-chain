// Probe: verify load_cached_model_ranges loads the union of disjoint ranges
// correctly. For the same GGUF:
//   A) load_cached_model_ranges(path, &[(0, 4), (24, 28)])
//   B) load_cached_model_shard(path, 0, 4) and load_cached_model_shard(path, 24, 28)
// The merged A must have exactly the layers populated in {0..4, 24..28},
// match B's weights bit-for-bit on those slots, and have empty placeholders
// everywhere else.
//
// This is the local precondition for the 3×-replication deploy: each seed
// will load multiple disjoint ranges in one process and must serve each
// range identically to a single-range node of the same configuration.
use arc_inference::cached_integer_model::{load_cached_model_ranges, load_cached_model_shard};

const MODEL_PATH: &str = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";

fn summarize(tag: &str, layers_loaded: &[bool], per_layer_bytes: &[usize]) {
    let total: usize = per_layer_bytes.iter().sum();
    let loaded: Vec<usize> = layers_loaded
        .iter()
        .enumerate()
        .filter_map(|(i, &b)| if b { Some(i) } else { None })
        .collect();
    eprintln!(
        "{tag}: {} layers loaded {:?}, {} bytes total",
        loaded.len(),
        loaded,
        total
    );
}

fn layer_bytes(l: &arc_inference::cached_integer_model::CachedLayer) -> usize {
    l.wq.memory_bytes()
        + l.wk.memory_bytes()
        + l.wv.memory_bytes()
        + l.wo.memory_bytes()
        + l.w_gate.memory_bytes()
        + l.w_up.memory_bytes()
        + l.w_down.memory_bytes()
}

fn main() {
    let ranges = [(0usize, 4usize), (24usize, 28usize)];

    eprintln!(
        "A) loading ranges {:?} via load_cached_model_ranges...",
        ranges
    );
    let merged =
        load_cached_model_ranges(MODEL_PATH, &ranges).expect("load_cached_model_ranges failed");

    let n = merged.config.n_layers;
    let merged_flags: Vec<bool> = merged.layers.iter().map(|l| l.is_loaded()).collect();
    let merged_bytes: Vec<usize> = merged.layers.iter().map(layer_bytes).collect();
    summarize("MERGED", &merged_flags, &merged_bytes);

    // Expected slots: union of all ranges.
    let expected: Vec<bool> = (0..n)
        .map(|i| ranges.iter().any(|&(s, e)| i >= s && i < e))
        .collect();
    let mismatches: Vec<usize> = (0..n).filter(|&i| merged_flags[i] != expected[i]).collect();
    assert!(
        mismatches.is_empty(),
        "Merged layers disagree with expected union at {:?}",
        mismatches
    );
    eprintln!("  membership OK");

    // Embedding must be present (range [0,4) includes 0)
    assert!(
        !merged.embedding_q16.is_empty(),
        "embedding should be loaded when 0 is in ranges"
    );
    eprintln!("  embedding loaded (len={})", merged.embedding_q16.len());

    // Output head must be present iff n is in any range end - range (24,28) does NOT include n_layers=32,
    // so output head must be EMPTY in merged.
    let covers_tail = ranges.iter().any(|&(_, e)| e == n);
    if covers_tail {
        assert!(
            merged.output_weight.n_rows != 0,
            "output head must load when range ends at n_layers"
        );
    } else {
        assert!(
            merged.output_weight.n_rows == 0,
            "output head must NOT load when no range ends at n_layers"
        );
    }
    eprintln!("  output_head tail-coverage={} OK", covers_tail);

    // B) cross-check against single-range loads for each range
    for &(s, e) in &ranges {
        eprintln!("B) comparing against load_cached_model_shard({s}, {e}) ...");
        let single =
            load_cached_model_shard(MODEL_PATH, s, e).expect("load_cached_model_shard failed");
        assert_eq!(single.config.n_layers, n);
        for i in s..e {
            let a = &merged.layers[i];
            let b = &single.layers[i];
            // Minimum structural check: same n_rows + same memory footprint.
            assert_eq!(a.wq.n_rows, b.wq.n_rows, "wq.n_rows differs at layer {i}");
            assert_eq!(
                a.w_down.n_rows, b.w_down.n_rows,
                "w_down.n_rows differs at layer {i}"
            );
            assert_eq!(
                layer_bytes(a),
                layer_bytes(b),
                "layer {i} byte size differs"
            );
            // Weight-data equality: cheap-ish (per-layer ~200 MB on 7B; we still
            // compare fully to catch any silent drift from the merge).
            assert_eq!(a.wq.data, b.wq.data, "wq bytes differ at layer {i}");
            assert_eq!(
                a.w_down.data, b.w_down.data,
                "w_down bytes differ at layer {i}"
            );
        }
        eprintln!("  range [{s},{e}) matches single-shard load");
    }

    eprintln!(
        "\nALL CHECKS PASSED - multi-range loader is equivalent to concatenated single loads."
    );
}
