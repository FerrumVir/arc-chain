# Tensor row layer bundles

`tensor_row_layer_bundle_export` creates a private sidecar bundle for the existing `tensor_row_stdio_worker`. It selects complete transformer layers, exporting all rows of each layer's seven projections (`wq`, `wk`, `wv`, `wo`, `w_gate`, `w_up`, `w_down`). It does not export the LM head for a partial worker. The exporter uses the canonical interleaved-RoPE loader and `export_verified_model_rows`, and refuses an artifact whose full-file BLAKE3 differs from the supplied commitment.

```sh
cargo run -p arc-inference --example tensor_row_layer_bundle_export \
  --features candle --release -- \
  --model /absolute/path/model.gguf \
  --expected-blake3 <64-hex-digits> \
  --worker-id sidecar-1 \
  --layers 0,1,4 \
  --output-dir /absolute/path/sidecar-1-bundle
```

The output directory is published atomically and must not already exist. It contains `manifest.json` and a `rows/` directory with only protocol row files; point `tensor_row_stdio_worker` at that `rows/` directory. Before writing any row file, the exporter validates tensor shapes and preflights the aggregate serialized row files against the worker's 1 GiB directory limit. The manifest records the artifact commitment, canonical execution profile, worker ID, selected layers, row-file hashes and byte counts, and a `row_cohort_worker_config_fragment`. Replace its transport placeholders and set measured `ram_headroom_bytes` before using the entry. The worker echoes the request's worker ID as a placement label; it does not authenticate that value.

This tool loads the complete GGUF into the coordinator/export process to hash and canonicalize it. `resident_layers` reduces the worker's row residency after export; it does not make model export low-memory and does not establish that a worker has sufficient RAM. Qualify capacity separately before deployment.
